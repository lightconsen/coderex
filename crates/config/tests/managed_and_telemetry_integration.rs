//! Integration tests for layered settings that carry security-relevant
//! values: `telemetry.strict` and the managed-settings carrier.

use std::path::PathBuf;

use coderex_config::ConfigService;
use coderex_config::dirs::ManagedSettings;

fn write(path: &PathBuf, json: &str) {
    std::fs::write(path, json).expect("write settings");
}

#[test]
fn telemetry_strict_reaches_the_resolved_settings() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("settings.json");
    write(
        &user,
        r#"{"telemetry": {"strict": true, "endpoint": "https://telemetry.example/events"}}"#,
    );

    let resolved = ConfigService::new(user, None).resolve().unwrap();
    let telemetry = resolved.telemetry.expect("telemetry block");
    assert_eq!(telemetry.strict, Some(true));
    assert_eq!(
        telemetry.endpoint.as_deref(),
        Some("https://telemetry.example/events")
    );
}

#[test]
fn managed_settings_win_over_user_and_project() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("settings.json");
    let project = dir.path().join("project.json");
    let managed = dir.path().join("managed.json");
    write(
        &user,
        r#"{"permission_mode": "bypass-permissions", "telemetry": {"strict": false}}"#,
    );
    write(&project, r#"{"permission_mode": "accept-edits"}"#);
    let managed_doc = ManagedSettings::new(serde_json::json!({
        "permission_mode": "plan",
        "telemetry": {"strict": true},
    }));
    write(
        &managed,
        &serde_json::to_string(&managed_doc).expect("managed document"),
    );

    let resolved = ConfigService::new(user, Some(project))
        .with_managed_settings_path(managed)
        .resolve()
        .unwrap();

    assert_eq!(resolved.permission_mode.as_deref(), Some("plan"));
    assert_eq!(
        resolved.telemetry.and_then(|telemetry| telemetry.strict),
        Some(true),
        "the managed layer is applied last and cannot be loosened below it"
    );
}

#[test]
fn a_missing_managed_file_leaves_the_user_settings_alone() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("settings.json");
    write(&user, r#"{"model": "claude-haiku-4-5-20251001"}"#);

    let resolved = ConfigService::new(user, None::<PathBuf>)
        .with_managed_settings_path(dir.path().join("absent.json"))
        .resolve()
        .unwrap();

    assert_eq!(resolved.model.as_deref(), Some("claude-haiku-4-5-20251001"));
}

#[test]
fn an_unknown_managed_key_is_rejected_rather_than_silently_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("settings.json");
    let managed = dir.path().join("managed.json");
    write(&user, r#"{"model": "claude-haiku-4-5-20251001"}"#);
    let managed_doc = ManagedSettings::new(serde_json::json!({"not_a_setting": true}));
    write(
        &managed,
        &serde_json::to_string(&managed_doc).expect("managed document"),
    );

    let service = ConfigService::new(user, None::<PathBuf>).with_managed_settings_path(managed);
    let error = service
        .load_managed_config()
        .expect_err("the carrier rejects unknown keys");
    assert!(
        format!("{error}").contains("not_a_setting"),
        "the error names the field: {error}"
    );

    // Resolution treats an unreadable managed file as absent: the user's
    // values stand, and nothing from the policy layer is half-applied.
    let resolved = service.resolve().expect("resolution continues");
    assert_eq!(resolved.model.as_deref(), Some("claude-haiku-4-5-20251001"));
}

// ── OS managed carriers ─────────────────────────────────────────────────────

fn write_plist(path: &PathBuf, entries: &[(&str, plist::Value)]) {
    let mut dict = plist::Dictionary::new();
    for (key, value) in entries {
        dict.insert((*key).to_string(), value.clone());
    }
    plist::Value::Dictionary(dict)
        .to_file_xml(path)
        .expect("write plist");
}

/// The plist carrier is merged after the managed JSON file, so an MDM-installed
/// profile wins over both the user's file and the file carrier.
#[test]
fn a_managed_plist_is_merged_over_user_and_file_settings() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("settings.json");
    let managed = dir.path().join("managed.json");
    let plist = dir.path().join("dev.coderex.plist");
    write(
        &user,
        r#"{"model": "user-model", "telemetry": {"strict": false}}"#,
    );
    let managed_doc = ManagedSettings::new(serde_json::json!({
        "model": "file-managed-model",
        "telemetry": {"strict": true},
    }));
    write(
        &managed,
        &serde_json::to_string(&managed_doc).expect("managed document"),
    );
    write_plist(
        &plist,
        &[
            ("model", plist::Value::String("plist-model".into())),
            ("telemetry_enabled", plist::Value::Boolean(false)),
        ],
    );

    let resolved = ConfigService::new(user, None::<PathBuf>)
        .with_managed_settings_path(managed)
        .with_managed_settings_plist(plist)
        .resolve()
        .unwrap();

    assert_eq!(resolved.model.as_deref(), Some("plist-model"));
    assert_eq!(resolved.telemetry_enabled, Some(false));
}

/// A plist over the settings size cap is skipped whole, exactly like the JSON
/// carrier: its otherwise-valid keys do not apply.
#[test]
fn a_managed_plist_over_the_size_limit_is_skipped_whole() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("settings.json");
    let plist = dir.path().join("dev.coderex.plist");
    write(&user, r#"{"model": "user-model"}"#);
    write_plist(
        &plist,
        &[
            ("model", plist::Value::String("plist-model".into())),
            (
                "api_key",
                plist::Value::String("a".repeat(2 * 1024 * 1024 + 1)),
            ),
        ],
    );

    let resolved = ConfigService::new(user, None::<PathBuf>)
        .with_managed_settings_plist(plist)
        .resolve()
        .unwrap();

    assert_eq!(
        resolved.model.as_deref(),
        Some("user-model"),
        "an oversized managed plist must not apply any of its keys"
    );
}
