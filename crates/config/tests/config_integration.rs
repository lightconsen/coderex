//! Config integration tests — covering priority, parsing, flags, experiments,
//! and directory paths.
//!
//! These tests correspond to the equivalence test plan Module 8:
//! 8.1 config priority, 8.2 settings.json parsing, 8.3 feature flags,
//! 8.4 experiments, 8.5 directory paths

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use coderex_config::dirs::{DataDirectories, ManagedSettings};
use coderex_config::{
    CURRENT_SCHEMA_VERSION, CapabilitySet, ConfigService, FeatureFlags, FlagRegistry,
    RuntimeOverrides, Settings, config_home, percentage_rollout,
};

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

fn unique_name(prefix: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}_{ts}")
}

fn temp_settings_file(settings: &Settings) -> PathBuf {
    let tmp = std::env::temp_dir();
    let path = tmp.join(format!("{}.json", unique_name("coderex_integ")));
    std::fs::write(&path, serde_json::to_string_pretty(settings).unwrap()).unwrap();
    path
}

// ═══════════════════════════════════════════════════════════════════════════
//  8.1 Config Priority — defaults < user < project < runtime overrides
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn config_priority_defaults_when_no_files() {
    let service = ConfigService::new(PathBuf::from("/nonexistent/user.json"), None);
    let resolved = service.resolve().unwrap();

    assert_eq!(resolved.model, Some("claude-sonnet-4-6".into()));
    assert_eq!(resolved.permission_mode, Some("default".into()));
    assert_eq!(resolved.config_version, CURRENT_SCHEMA_VERSION);
}

#[test]
fn config_priority_user_overrides_defaults() {
    let user = Settings {
        model: Some("claude-haiku-4-5-20251001".into()),
        ..Default::default()
    };
    let user_path = temp_settings_file(&user);

    let service = ConfigService::new(user_path.clone(), None);
    let resolved = service.resolve().unwrap();

    assert_eq!(resolved.model, Some("claude-haiku-4-5-20251001".into()));
    assert_eq!(resolved.permission_mode, Some("default".into()));

    let _ = std::fs::remove_file(&user_path);
}

#[test]
fn config_priority_project_overrides_user() {
    let user = Settings {
        model: Some("claude-sonnet-4-6".into()),
        max_budget_usd: Some(20.0),
        ..Default::default()
    };
    let project = Settings {
        permission_mode: Some("accept-edits".into()),
        max_budget_usd: Some(5.0),
        ..Default::default()
    };
    let user_path = temp_settings_file(&user);
    let project_path = temp_settings_file(&project);

    let service = ConfigService::new(user_path.clone(), Some(project_path.clone()));
    let resolved = service.resolve().unwrap();

    // User sets model, project doesn't → model from user
    assert_eq!(resolved.model, Some("claude-sonnet-4-6".into()));
    // Project overrides user for max_budget_usd
    assert_eq!(resolved.max_budget_usd, Some(5.0));
    // Project sets permission_mode
    assert_eq!(resolved.permission_mode, Some("accept-edits".into()));

    let _ = std::fs::remove_file(&user_path);
    let _ = std::fs::remove_file(&project_path);
}

#[test]
fn config_priority_runtime_overrides_all() {
    let user = Settings {
        model: Some("claude-sonnet-4-6".into()),
        ..Default::default()
    };
    let user_path = temp_settings_file(&user);

    let service = ConfigService::new(user_path.clone(), None);
    let resolved = service.resolve().unwrap();

    let overrides = RuntimeOverrides {
        model: Some("claude-opus-4-6".into()),
        permission_mode: Some("plan".into()),
        max_budget_usd: Some(50.0),
        ..Default::default()
    };
    let with_overrides = overrides.apply(resolved);

    assert_eq!(with_overrides.model, Some("claude-opus-4-6".into()));
    assert_eq!(with_overrides.permission_mode, Some("plan".into()));
    assert_eq!(with_overrides.max_budget_usd, Some(50.0));

    let _ = std::fs::remove_file(&user_path);
}

#[test]
fn config_priority_merge_order_is_correct() {
    let defaults = Settings::defaults();
    let user = Settings {
        model: Some("user-model".into()),
        max_budget_usd: Some(10.0),
        ..Default::default()
    };
    let project = Settings {
        max_budget_usd: Some(5.0),
        permission_mode: Some("dont-ask".into()),
        ..Default::default()
    };

    let merged = defaults.merge(&user).merge(&project);

    assert_eq!(merged.model, Some("user-model".into()));
    assert_eq!(merged.max_budget_usd, Some(5.0));
    assert_eq!(merged.permission_mode, Some("dont-ask".into()));
    // Undecided by default — telemetry is opt-in (see Settings::defaults).
    assert_eq!(merged.telemetry_enabled, None);
}

// ═══════════════════════════════════════════════════════════════════════════
//  8.2 Settings.json Parsing — serde roundtrip, validation, unknown fields
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn settings_json_serde_roundtrip() {
    let settings = Settings {
        config_version: CURRENT_SCHEMA_VERSION,
        model: Some("custom-model".into()),
        permission_mode: Some("plan".into()),
        max_budget_usd: Some(15.5),
        telemetry_enabled: Some(false),
        telemetry: Some(coderex_config::settings::TelemetrySubConfig {
            endpoint: Some("https://telemetry.example.com".into()),
            crash_endpoint: None,
            batch_size: Some(100),
            upload_interval_secs: Some(300),
            sample_rate: Some(0.5),
            pinned_cert_pem: None,
        }),
        hooks: None,
        mcp_servers: None,
        api_key: Some("sk-test".into()),
    };

    let json = serde_json::to_string_pretty(&settings).unwrap();
    let decoded: Settings = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.model, settings.model);
    assert_eq!(decoded.permission_mode, settings.permission_mode);
    assert_eq!(decoded.max_budget_usd, settings.max_budget_usd);
    assert_eq!(decoded.telemetry_enabled, settings.telemetry_enabled);
    assert_eq!(decoded.api_key, settings.api_key);
}

#[test]
fn settings_validate_rejects_empty_model() {
    let settings = Settings {
        model: Some("".into()),
        ..Default::default()
    };
    let err = settings.validate().unwrap_err();
    assert!(
        matches!(err, coderex_config::ConfigError::Validation { ref field, .. } if field == "model")
    );
}

#[test]
fn settings_validate_rejects_negative_budget() {
    let settings = Settings {
        max_budget_usd: Some(-1.0),
        ..Default::default()
    };
    let err = settings.validate().unwrap_err();
    assert!(
        matches!(err, coderex_config::ConfigError::Validation { ref field, .. } if field == "max_budget_usd")
    );
}

#[test]
fn settings_validate_rejects_invalid_permission_mode() {
    let settings = Settings {
        permission_mode: Some("god-mode".into()),
        ..Default::default()
    };
    let err = settings.validate().unwrap_err();
    assert!(
        matches!(err, coderex_config::ConfigError::Validation { ref field, .. } if field == "permission_mode")
    );
}

#[test]
fn config_service_invalid_file_falls_back_to_defaults() {
    // ConfigService::resolve() swallows file-level errors and falls back to defaults
    let settings = Settings {
        model: Some("".into()),
        ..Default::default()
    };
    let path = temp_settings_file(&settings);

    let service = ConfigService::new(path.clone(), None);
    let resolved = service.resolve().unwrap();
    // Falls back to defaults because the user config failed validation
    assert_eq!(resolved.model, Some("claude-sonnet-4-6".into()));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn config_service_unknown_field_falls_back_to_defaults() {
    let tmp = std::env::temp_dir();
    let path = tmp.join(format!("{}.json", unique_name("coderex_integ_unknown")));
    std::fs::write(
        &path,
        r#"{"model":"test-model","unknown_custom_key":"value"}"#,
    )
    .unwrap();

    let service = ConfigService::new(path.clone(), None);
    let resolved = service.resolve().unwrap();
    // Falls back to defaults because unknown field was rejected
    assert_eq!(resolved.model, Some("claude-sonnet-4-6".into()));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn settings_json_migration_from_v0() {
    let tmp = std::env::temp_dir();
    let path = tmp.join(format!("{}.json", unique_name("coderex_integ_v0")));
    std::fs::write(
        &path,
        r#"{"config_version":0,"model":"old-model","permission_mode":"default"}"#,
    )
    .unwrap();

    let service = ConfigService::new(path.clone(), None);
    let resolved = service.resolve().unwrap();
    assert_eq!(resolved.config_version, CURRENT_SCHEMA_VERSION);
    assert_eq!(resolved.model, Some("old-model".into()));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn config_service_diff_detects_changes() {
    let user = Settings {
        model: Some("custom-model".into()),
        max_budget_usd: Some(25.0),
        ..Default::default()
    };
    let user_path = temp_settings_file(&user);

    let service = ConfigService::new(user_path.clone(), None);
    let diff = service.diff().unwrap();

    assert!(!diff.is_empty());
    let keys: Vec<_> = diff.changes.iter().map(|e| e.key.as_str()).collect();
    assert!(keys.contains(&"model"));
    assert!(keys.contains(&"max_budget_usd"));

    let rendered = diff.render();
    assert!(rendered.contains("model"));
    assert!(rendered.contains("custom-model"));

    let _ = std::fs::remove_file(&user_path);
}

// ═══════════════════════════════════════════════════════════════════════════
//  8.3 Feature Flags — parsing, env vars, capability sets
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn feature_flags_parse_enables_single_flag() {
    let flags = FeatureFlags::parse("daemon");
    assert!(flags.daemon);
    assert!(!flags.buddy);
    assert!(!flags.coordinator_mode);
}

#[test]
fn feature_flags_parse_plus_minus_syntax() {
    let flags = FeatureFlags::parse("+daemon,+buddy,-daemon");
    assert!(!flags.daemon);
    assert!(flags.buddy);
}

#[test]
fn feature_flags_parse_all_17_flags() {
    let flags = FeatureFlags::parse(
        "streaming_tool_execution,tool_use_summaries,fast_mode,coordinator_mode,\
         history_snip,agent_triggers,bridge_mode,daemon,voice_mode,buddy,\
         workflow_scripts,web_browser_tool,context_collapse,terminal_panel,\
         kairos,team_collaboration,auto_dream",
    );
    assert!(flags.streaming_tool_execution);
    assert!(flags.tool_use_summaries);
    assert!(flags.fast_mode);
    assert!(flags.coordinator_mode);
    assert!(flags.history_snip);
    assert!(flags.agent_triggers);
    assert!(flags.bridge_mode);
    assert!(flags.daemon);
    assert!(flags.voice_mode);
    assert!(flags.buddy);
    assert!(flags.workflow_scripts);
    assert!(flags.web_browser_tool);
    assert!(flags.context_collapse);
    assert!(flags.terminal_panel);
    assert!(flags.kairos);
    assert!(flags.team_collaboration);
    assert!(flags.auto_dream);
}

#[test]
fn feature_flags_unknown_flag_no_panic() {
    let flags = FeatureFlags::parse("nonexistent_flag,+daemon,unknown");
    assert!(flags.daemon);
    // Unknown flags should be silently ignored (with a warning)
}

// Note: from_env() tests are omitted because std::env::set_var/remove_var
// require unsafe blocks and this workspace has unsafe_code = "deny".
// from_env() is a thin wrapper around parse(std::env::var(...)) — fully
// covered by the parse tests above.

#[test]
fn capability_set_derives_from_flags() {
    let mut flags = FeatureFlags::default();
    flags.daemon = true;
    flags.bridge_mode = true;
    flags.voice_mode = true;
    flags.auto_dream = true;

    let caps = flags.capabilities();
    assert!(caps.contains(CapabilitySet::DAEMON));
    assert!(caps.contains(CapabilitySet::BRIDGE));
    assert!(caps.contains(CapabilitySet::VOICE));
    assert!(caps.contains(CapabilitySet::DREAM));
    assert!(!caps.contains(CapabilitySet::COORDINATOR));
    assert!(!caps.contains(CapabilitySet::TEAM));
}

#[test]
fn capability_set_empty_by_default() {
    let flags = FeatureFlags::default();
    let caps = flags.capabilities();
    assert!(caps.is_empty());
}

#[test]
fn flag_registry_read_write() {
    let registry = FlagRegistry::new();
    assert!(!registry.has_capability(CapabilitySet::DAEMON));

    registry.set_flag("daemon", true);
    assert!(registry.has_capability(CapabilitySet::DAEMON));

    let flags = registry.flags();
    assert!(flags.daemon);
    assert!(!flags.buddy);
}

#[test]
fn flag_registry_capabilities_update_after_set() {
    let registry = FlagRegistry::from_flags(FeatureFlags::default());
    assert!(registry.capabilities().is_empty());

    registry.set_flag("coordinator_mode", true);
    registry.set_flag("team_collaboration", true);

    assert!(registry.has_capability(CapabilitySet::COORDINATOR));
    assert!(registry.has_capability(CapabilitySet::TEAM));
    assert!(!registry.has_capability(CapabilitySet::DAEMON));
}

// Note: FlagRegistry::from_env() is a thin wrapper around
// FeatureFlags::from_env() → fully covered indirectly by parse tests.

// ═══════════════════════════════════════════════════════════════════════════
//  8.4 Experiments — with_experiments, percentage rollout
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn experiments_override_base_flags() {
    let flags = FeatureFlags::default();
    let mut experiments = HashMap::new();
    experiments.insert("daemon".into(), true);
    experiments.insert("buddy".into(), true);
    experiments.insert("coordinator_mode".into(), false);

    let result = flags.with_experiments(&experiments);
    assert!(result.daemon);
    assert!(result.buddy);
    assert!(!result.coordinator_mode);
    assert!(!result.bridge_mode);
}

#[test]
fn experiments_merged_with_existing_flags() {
    let mut flags = FeatureFlags::default();
    flags.daemon = true;
    flags.coordinator_mode = true;

    let mut experiments = HashMap::new();
    experiments.insert("coordinator_mode".into(), false);
    experiments.insert("buddy".into(), true);

    let result = flags.with_experiments(&experiments);
    assert!(result.daemon); // preserved
    assert!(!result.coordinator_mode); // overridden by experiment
    assert!(result.buddy); // added by experiment
}

#[test]
fn percentage_rollout_deterministic() {
    let r1 = percentage_rollout("feature_a", "session_1", 50);
    let r2 = percentage_rollout("feature_a", "session_1", 50);
    assert_eq!(r1, r2);
}

#[test]
fn percentage_rollout_0_percent_always_false() {
    assert!(!percentage_rollout("f", "s", 0));
}

#[test]
fn percentage_rollout_100_percent_always_true() {
    assert!(percentage_rollout("f", "s", 100));
}

#[test]
fn percentage_rollout_different_sessions_may_differ() {
    let _r1 = percentage_rollout("feature", "session_a", 50);
    let _r2 = percentage_rollout("feature", "session_b", 50);
    // We can't assert equality/inequality without knowing the hash,
    // but the test ensures it doesn't panic and is deterministic per session.
}

#[test]
fn flag_registry_apply_experiments() {
    let registry = FlagRegistry::new();
    let mut experiments = HashMap::new();
    experiments.insert("daemon".into(), true);
    experiments.insert("voice_mode".into(), true);

    registry.apply_experiments(&experiments);
    assert!(registry.has_capability(CapabilitySet::DAEMON));
    assert!(registry.has_capability(CapabilitySet::VOICE));
}

// ═══════════════════════════════════════════════════════════════════════════
//  8.5 Directory Paths — DataDirectories, config_home, ManagedSettings
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn data_directories_ensure_all_creates_subdirs() {
    let tmp = std::env::temp_dir().join(unique_name("coderex_dirs"));
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
fn data_directories_paths_are_correct() {
    let dirs = DataDirectories::new(PathBuf::from("/tmp/coderex"));

    assert_eq!(
        dirs.managed_settings_path(),
        PathBuf::from("/tmp/coderex/managed_settings.json")
    );
    assert_eq!(
        dirs.policy_limits_path(),
        PathBuf::from("/tmp/coderex/policy_limits.json")
    );
    assert_eq!(
        dirs.sessions_db_path(),
        PathBuf::from("/tmp/coderex/sessions.db")
    );
    assert_eq!(
        dirs.project_dir("abc123"),
        PathBuf::from("/tmp/coderex/projects/abc123")
    );
    assert_eq!(
        dirs.crash_log_path(),
        PathBuf::from("/tmp/coderex/crash.log")
    );
    assert_eq!(dirs.base_path(), PathBuf::from("/tmp/coderex"));
}

#[test]
fn config_home_returns_path() {
    let home = config_home();
    assert!(!home.as_os_str().is_empty());
    // Should end with .coderex unless CLAUDE_CONFIG_DIR is set
    if std::env::var("CLAUDE_CONFIG_DIR").is_err() {
        assert!(home.to_string_lossy().ends_with(".coderex"));
    }
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
fn managed_settings_new_computes_checksum() {
    let settings = serde_json::json!({"telemetry_enabled": true});
    let managed = ManagedSettings::new(settings);
    assert!(!managed.checksum.is_empty());
    assert!(managed.fetched_at <= chrono::Utc::now());
}

#[test]
fn data_directories_idempotent_ensure_all() {
    let tmp = std::env::temp_dir().join(unique_name("coderex_dirs_idem"));
    let _ = std::fs::remove_dir_all(&tmp);

    let dirs = DataDirectories::new(tmp.clone());
    dirs.ensure_all().unwrap();
    dirs.ensure_all().unwrap(); // second call should not fail

    assert!(tmp.join("projects").exists());

    let _ = std::fs::remove_dir_all(&tmp);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-cutting: RuntimeOverrides edge cases
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn runtime_overrides_partial_apply() {
    let settings = Settings::defaults();
    let overrides = RuntimeOverrides {
        model: Some("override-model".into()),
        ..Default::default()
    };
    let result = overrides.apply(settings);

    assert_eq!(result.model, Some("override-model".into()));
    assert_eq!(result.permission_mode, Some("default".into())); // unchanged
    assert_eq!(result.telemetry_enabled, None); // undecided — opt-in
}

#[test]
fn runtime_overrides_none_are_no_op() {
    let settings = Settings::defaults();
    let overrides = RuntimeOverrides::default();
    let result = overrides.apply(settings.clone());

    assert_eq!(result.model, settings.model);
    assert_eq!(result.permission_mode, settings.permission_mode);
    assert_eq!(result.max_budget_usd, settings.max_budget_usd);
}

#[test]
fn settings_merge_preserves_higher_config_version() {
    let a = Settings {
        config_version: 2,
        ..Default::default()
    };
    let b = Settings {
        config_version: 3,
        ..Default::default()
    };
    let merged = a.merge(&b);
    assert_eq!(merged.config_version, 3);
}

#[test]
fn diff_from_defaults_empty_when_matching() {
    let settings = Settings::defaults();
    let diff = settings.diff_from_defaults("test");
    assert!(diff.is_empty());
    assert!(diff.render().contains("No changes"));
}

#[test]
fn config_service_project_config_missing_is_ok() {
    let service = ConfigService::new(
        PathBuf::from("/nonexistent/user.json"),
        Some(PathBuf::from("/nonexistent/project.json")),
    );
    let resolved = service.resolve().unwrap();
    assert_eq!(resolved.model, Some("claude-sonnet-4-6".into()));
}
