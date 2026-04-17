//! CodeRex configuration system.

#![warn(missing_docs)]

pub mod settings;
pub mod flags;
pub mod experiments;
pub mod dirs;

#[cfg(feature = "enterprise")]
pub mod remote_settings;
#[cfg(feature = "enterprise")]
pub mod trusted_device;
#[cfg(feature = "enterprise")]
pub mod org_policy;

#[cfg(feature = "enterprise")]
pub use remote_settings::{
    FetchState, LoadingBarrier, RemoteManagedSettings, RemoteManagedSettingsClient,
    RemoteSettingsError, RemoteSettingsManager, RemoteSettingsResponse,
};
pub use settings::{config_home, ConfigService, ConfigError, RuntimeOverrides, Settings};
#[cfg(feature = "enterprise")]
pub use org_policy::{OrgPolicy, OrgPolicyError, OrgPolicyStore};
#[cfg(feature = "enterprise")]
pub use trusted_device::{
    machine_fingerprint, default_device_store_path, DeviceRecord, TrustStatus,
    TrustedDeviceError, TrustedDeviceRegistry,
};
