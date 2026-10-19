//! OS-level managed-settings carriers.
//!
//! These carriers complement the managed JSON file in
//! [`crate::settings::ConfigService`]:
//!
//! - **macOS**: a property list installed by an MDM profile, conventionally at
//!   `/Library/Managed Preferences/<bundle-id>.plist`.
//! - **Windows**: values under `HKLM\SOFTWARE\CodeRex`.
//!
//! Both are administrator-controlled, so the resulting [`Settings`] document is
//! finalized as a `"managed"` source: it may enable telemetry and
//! security-sensitive permission modes (unlike repository-controlled project
//! settings). The OS protects these locations, so — unlike the managed JSON
//! file — no checksum is verified.
//!
//! The plist parser is portable and is exercised by tests on macOS. The
//! registry reader is compiled only on Windows and is unverified on a macOS
//! development machine; only its pure value-conversion layer is unit-tested
//! everywhere.

use crate::settings::{
    ConfigError, Settings, check_unknown_fields, enforce_size_cap, finalize_settings,
};
use std::path::{Path, PathBuf};

/// Default bundle-like domain for the macOS managed-preferences plist.
pub const DEFAULT_MACOS_MANAGED_PLIST_DOMAIN: &str = "dev.coderex";

/// Default Windows registry key, relative to `HKEY_LOCAL_MACHINE`.
pub const WINDOWS_MANAGED_REGISTRY_KEY: &str = r"SOFTWARE\CodeRex";

/// Path of the system-wide macOS managed-preferences plist for `domain`.
///
/// MDM-installed profiles normally land in `/Library/Managed Preferences`; the
/// per-user `~/Library/Managed Preferences` location is not consulted because
/// managed policy must not be user-writable.
#[must_use]
pub fn default_managed_plist_path(domain: &str) -> PathBuf {
    Path::new("/Library/Managed Preferences").join(format!("{domain}.plist"))
}

/// Load a managed-settings plist.
///
/// Returns `Ok(None)` when the file does not exist. A plist whose top level is
/// not a dictionary, carries an unknown settings key, or fails validation is
/// reported as an error; the caller logs and skips this carrier.
pub(crate) fn load_managed_plist(path: &Path) -> Result<Option<Settings>, ConfigError> {
    if !path.exists() {
        return Ok(None);
    }
    enforce_size_cap(path)?;

    let value = plist::Value::from_file(path).map_err(|e| ConfigError::Plist {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let json = serde_json::to_value(&value).map_err(|source| ConfigError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    check_unknown_fields(&json, "managed plist")?;
    let settings: Settings = serde_json::from_value(json).map_err(|source| ConfigError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(Some(finalize_settings(settings, "managed")?))
}

/// A registry value converted to a platform-independent shape.
///
/// The conversion layer is compiled on Windows and in tests so the mapping can
/// be exercised without touching a real registry.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegistryValue {
    /// `REG_SZ` / `REG_EXPAND_SZ`.
    String(String),
    /// `REG_DWORD`.
    U32(u32),
    /// `REG_QWORD`.
    U64(u64),
    /// `REG_MULTI_SZ`.
    MultiString(Vec<String>),
}

/// Convert registry values into a settings JSON object.
///
/// Most values map by type: strings to strings, `REG_DWORD`/`REG_QWORD` to
/// numbers, and `REG_MULTI_SZ` to arrays. Structured keys (`telemetry`,
/// `hooks`, `mcp_servers`, `context_window`) are strings containing JSON and
/// are parsed. Two scalar keys are normalized because the registry has no
/// boolean or float type:
///
/// - `telemetry_enabled` accepts `REG_DWORD` (`0`/non-zero) or the strings
///   `true`/`false`/`1`/`0`.
/// - `config_version` accepts a numeric string and `max_budget_usd` a decimal
///   string.
///
/// An unparseable value is an error rather than being silently dropped, so an
/// administrator sees that the policy did not apply.
#[cfg(any(windows, test))]
pub(crate) fn registry_values_to_json(
    values: &[(String, RegistryValue)],
) -> Result<serde_json::Map<String, serde_json::Value>, ConfigError> {
    use serde_json::json;

    let mut object = serde_json::Map::new();
    for (name, value) in values {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let converted = match value {
            RegistryValue::String(raw) => registry_string_value(name, raw)?,
            RegistryValue::U32(number) if name == "telemetry_enabled" => json!(*number != 0),
            RegistryValue::U32(number) => json!(number),
            RegistryValue::U64(number) => json!(number),
            RegistryValue::MultiString(items) => json!(items),
        };
        object.insert(name.to_string(), converted);
    }
    Ok(object)
}

#[cfg(any(windows, test))]
fn registry_string_value(name: &str, raw: &str) -> Result<serde_json::Value, ConfigError> {
    use serde_json::json;

    match name {
        "telemetry_enabled" => parse_registry_bool(raw).map(|enabled| json!(enabled)),
        "config_version" => raw
            .trim()
            .parse::<u32>()
            .map(|version| json!(version))
            .map_err(|_| registry_validation(name, "must be an unsigned integer")),
        "max_budget_usd" => raw
            .trim()
            .parse::<f64>()
            .map(|budget| json!(budget))
            .map_err(|_| registry_validation(name, "must be a decimal number")),
        "telemetry" | "hooks" | "mcp_servers" | "context_window" => serde_json::from_str(raw)
            .map_err(|e| registry_validation(name, format!("must contain a JSON value: {e}"))),
        _ => Ok(json!(raw)),
    }
}

#[cfg(any(windows, test))]
fn parse_registry_bool(raw: &str) -> Result<bool, ConfigError> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("true") || trimmed == "1" {
        Ok(true)
    } else if trimmed.eq_ignore_ascii_case("false") || trimmed == "0" {
        Ok(false)
    } else {
        Err(registry_validation(
            "telemetry_enabled",
            "must be true/false or 1/0",
        ))
    }
}

#[cfg(any(windows, test))]
fn registry_validation(name: &str, message: impl Into<String>) -> ConfigError {
    ConfigError::Validation {
        field: format!(r"HKLM\{WINDOWS_MANAGED_REGISTRY_KEY}\{name}"),
        message: message.into(),
    }
}

/// Read managed settings from `HKEY_LOCAL_MACHINE\SOFTWARE\CodeRex`.
///
/// Returns `Ok(None)` when the key is absent. Supports `REG_SZ`,
/// `REG_EXPAND_SZ`, `REG_DWORD`, `REG_QWORD` and `REG_MULTI_SZ`; other value
/// types are ignored.
///
/// **Unverified:** this path is compile-gated to Windows and has not been run
/// on the macOS development machine that added it. See `docs/config.md`.
#[cfg(windows)]
pub(crate) fn load_managed_registry(key_path: &str) -> Result<Option<Settings>, ConfigError> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};
    use winreg::types::FromRegValue;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key = match hklm.open_subkey_with_flags(key_path, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ConfigError::Io(error)),
    };

    let mut values = Vec::new();
    for item in key.enum_values() {
        let (name, value) = item?;
        // `vtype` is not `Copy`; clone it so the value can still be borrowed.
        let converted = match value.vtype.clone() {
            winreg::enums::REG_SZ | winreg::enums::REG_EXPAND_SZ => {
                RegistryValue::String(String::from_reg_value(&value)?)
            }
            winreg::enums::REG_DWORD => RegistryValue::U32(u32::from_reg_value(&value)?),
            winreg::enums::REG_QWORD => RegistryValue::U64(u64::from_reg_value(&value)?),
            winreg::enums::REG_MULTI_SZ => {
                RegistryValue::MultiString(Vec::<String>::from_reg_value(&value)?)
            }
            _ => continue,
        };
        values.push((name, converted));
    }

    let json = serde_json::Value::Object(registry_values_to_json(&values)?);
    check_unknown_fields(&json, "managed registry")?;
    let settings: Settings = serde_json::from_value(json).map_err(|source| ConfigError::Json {
        path: PathBuf::from(format!(r"HKLM\{key_path}")),
        source,
    })?;
    Ok(Some(finalize_settings(settings, "managed")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── macOS plist carrier ──────────────────────────────────────────────

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_carrier_loads_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dev.coderex.plist");
        let mut dict = plist::Dictionary::new();
        dict.insert("model".into(), plist::Value::String("plist-model".into()));
        dict.insert("telemetry_enabled".into(), plist::Value::Boolean(true));
        plist::Value::Dictionary(dict).to_file_xml(&path).unwrap();

        let settings = load_managed_plist(&path).unwrap().unwrap();
        assert_eq!(settings.model.as_deref(), Some("plist-model"));
        assert_eq!(settings.telemetry_enabled, Some(true));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_carrier_rejects_unknown_keys() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("dev.coderex.plist");
        let mut dict = plist::Dictionary::new();
        dict.insert("model".into(), plist::Value::String("plist-model".into()));
        dict.insert("not_a_setting".into(), plist::Value::String("value".into()));
        plist::Value::Dictionary(dict).to_file_xml(&path).unwrap();

        let error = load_managed_plist(&path).unwrap_err();
        assert!(
            matches!(error, ConfigError::UnknownField { ref field } if field == "not_a_setting"),
            "{error}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_carrier_missing_file_is_absent() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            load_managed_plist(&temp.path().join("missing.plist"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn default_macos_plist_path_is_the_system_managed_location() {
        assert_eq!(
            default_managed_plist_path("dev.coderex"),
            PathBuf::from("/Library/Managed Preferences/dev.coderex.plist")
        );
    }

    // ── Windows registry value mapping (pure, runs on every platform) ─────

    #[test]
    fn registry_values_map_to_settings_json() {
        let values = vec![
            (
                "model".to_string(),
                RegistryValue::String("registry-model".to_string()),
            ),
            ("telemetry_enabled".to_string(), RegistryValue::U32(0)),
            ("config_version".to_string(), RegistryValue::U32(4)),
            (
                "max_budget_usd".to_string(),
                RegistryValue::String("12.5".to_string()),
            ),
            (
                "context_window".to_string(),
                RegistryValue::String(r#"{"default":64000}"#.to_string()),
            ),
            (
                "mcp_servers".to_string(),
                RegistryValue::MultiString(vec!["a".to_string(), "b".to_string()]),
            ),
        ];

        let json = serde_json::Value::Object(registry_values_to_json(&values).unwrap());
        let settings: Settings = serde_json::from_value(json).unwrap();
        assert_eq!(settings.model.as_deref(), Some("registry-model"));
        assert_eq!(settings.telemetry_enabled, Some(false));
        assert_eq!(settings.config_version, 4);
        assert_eq!(settings.max_budget_usd, Some(12.5));
        assert_eq!(settings.context_window.unwrap().default, Some(64_000));
        assert!(settings.mcp_servers.is_some());
    }

    #[test]
    fn registry_telemetry_enabled_accepts_string_forms() {
        for (raw, expected) in [("true", true), ("1", true), ("false", false), ("0", false)] {
            let values = vec![(
                "telemetry_enabled".to_string(),
                RegistryValue::String(raw.to_string()),
            )];
            let json = serde_json::Value::Object(registry_values_to_json(&values).unwrap());
            let settings: Settings = serde_json::from_value(json).unwrap();
            assert_eq!(settings.telemetry_enabled, Some(expected), "{raw}");
        }
    }

    #[test]
    fn registry_invalid_values_are_rejected() {
        let values = vec![(
            "telemetry_enabled".to_string(),
            RegistryValue::String("maybe".to_string()),
        )];
        assert!(registry_values_to_json(&values).is_err());

        let values = vec![(
            "max_budget_usd".to_string(),
            RegistryValue::String("not-a-number".to_string()),
        )];
        assert!(registry_values_to_json(&values).is_err());
    }

    #[test]
    fn registry_qword_maps_to_a_number() {
        let values = vec![("config_version".to_string(), RegistryValue::U64(4))];
        let json = serde_json::Value::Object(registry_values_to_json(&values).unwrap());
        assert_eq!(json["config_version"], serde_json::json!(4));
    }

    #[test]
    fn registry_key_constant_matches_the_documented_location() {
        assert_eq!(WINDOWS_MANAGED_REGISTRY_KEY, r"SOFTWARE\CodeRex");
    }

    // ── Windows registry carrier (real HKLM) ─────────────────────────────
    //
    // The step used to check only that the constant and the carrier selection
    // were right; `load_managed_registry` itself was never called. These tests
    // write a throwaway key under HKLM and delete it again on drop.

    /// Whether CI requires the registry tests to really run.
    #[cfg(windows)]
    fn require_hklm_tests() -> bool {
        std::env::var("CODEREX_REQUIRE_HKLM_TESTS").as_deref() == Ok("1")
    }

    #[cfg(windows)]
    struct ScratchKey {
        path: String,
        hive: winreg::HKEY,
    }

    #[cfg(windows)]
    impl ScratchKey {
        fn new(hive: winreg::HKEY, root: &str) -> std::io::Result<Self> {
            use winreg::enums::{KEY_ALL_ACCESS, KEY_READ};
            use winreg::RegKey;

            let path = format!(
                r"{root}\Tests\coderex-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| elapsed.as_nanos())
            );
            RegKey::predef(hive).create_subkey_with_flags(&path, KEY_ALL_ACCESS | KEY_READ)?;
            Ok(Self { path, hive })
        }

        fn key(&self) -> winreg::RegKey {
            use winreg::enums::{KEY_ALL_ACCESS, KEY_READ};
            use winreg::RegKey;

            RegKey::predef(self.hive)
                .open_subkey_with_flags(&self.path, KEY_ALL_ACCESS | KEY_READ)
                .expect("the scratch key was just created")
        }
    }

    #[cfg(windows)]
    impl Drop for ScratchKey {
        fn drop(&mut self) {
            use winreg::RegKey;

            let _ = RegKey::predef(self.hive).delete_subkey_all(&self.path);
        }
    }

    /// Read a real key under HKLM: every supported value type crosses into
    /// `Settings`, including the DWORD and QWORD shapes.
    #[cfg(windows)]
    #[test]
    fn registry_carrier_reads_hklm() {
        use winreg::enums::HKEY_LOCAL_MACHINE;

        let scratch = match ScratchKey::new(HKEY_LOCAL_MACHINE, r"SOFTWARE\CodeRex") {
            Ok(scratch) => scratch,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                assert!(
                    !require_hklm_tests(),
                    "HKLM is not writable on this runner but CODEREX_REQUIRE_HKLM_TESTS=1"
                );
                eprintln!("skipping: this process cannot write HKLM ({error})");
                return;
            }
            Err(error) => panic!("could not create the HKLM scratch key: {error}"),
        };

        let key = scratch.key();
        key.set_value("model", &"managed-model").unwrap();
        key.set_value("telemetry_enabled", &0u32).unwrap();
        key.set_value("config_version", &7u64).unwrap();
        key.set_value("max_budget_usd", &"12.5").unwrap();

        let settings = load_managed_registry(&scratch.path)
            .expect("the carrier should read the key")
            .expect("the key exists");
        assert_eq!(settings.model.as_deref(), Some("managed-model"));
        assert_eq!(settings.telemetry_enabled, Some(false));
        assert_eq!(settings.config_version, 7);
        assert_eq!(settings.max_budget_usd, Some(12.5));
    }

    /// The carrier reads HKLM, not the user's hive: the same relative path
    /// under HKCU must not be found.
    #[cfg(windows)]
    #[test]
    fn registry_carrier_ignores_hkcu() {
        use winreg::enums::HKEY_CURRENT_USER;

        let scratch = ScratchKey::new(HKEY_CURRENT_USER, r"SOFTWARE\CodeRex")
            .expect("HKCU is always writable");
        scratch.key().set_value("model", &"user-model").unwrap();

        let found = load_managed_registry(&scratch.path).expect("HKLM lookup should succeed");
        assert!(
            found.is_none(),
            "an HKCU value must not be read as managed settings"
        );
    }

    /// A value the settings schema does not know is rejected, not silently
    /// dropped.
    #[cfg(windows)]
    #[test]
    fn registry_carrier_rejects_an_unknown_value() {
        use winreg::enums::HKEY_LOCAL_MACHINE;

        let scratch = match ScratchKey::new(HKEY_LOCAL_MACHINE, r"SOFTWARE\CodeRex") {
            Ok(scratch) => scratch,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                assert!(
                    !require_hklm_tests(),
                    "HKLM is not writable on this runner but CODEREX_REQUIRE_HKLM_TESTS=1"
                );
                eprintln!("skipping: this process cannot write HKLM ({error})");
                return;
            }
            Err(error) => panic!("could not create the HKLM scratch key: {error}"),
        };

        scratch.key().set_value("not_a_setting", &"x").unwrap();
        assert!(matches!(
            load_managed_registry(&scratch.path),
            Err(ConfigError::UnknownField { .. })
        ));
    }
}
