//! Plugin host settings are read from the raw settings document, not from the
//! resolved `Settings` struct (which does not model them).

use std::path::PathBuf;

use coderex_config::ConfigService;

#[test]
fn plugin_options_are_read_from_the_user_settings_file() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("settings.json");
    std::fs::write(
        &user,
        r#"{"config_version":4,"plugins":{"mock-plugin":{"options":{"GREETING":"hi"}}}}"#,
    )
    .unwrap();

    let host = ConfigService::new(user, None::<PathBuf>)
        .with_local_config_path(dir.path().join("settings.local.json"))
        .with_platform_managed_settings()
        .load_plugin_host_config();

    let plugins = host.plugins.expect("plugins were not read");
    assert_eq!(plugins["mock-plugin"]["options"]["GREETING"], "hi");
}

#[test]
fn loading_current_settings_does_not_rewrite_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("settings.json");
    let original = r#"{"config_version":4,"model":"claude-haiku-4-5-20251001","plugins":{"p":{"options":{"K":"v"}}},"marketplaces":[{"name":"m","url":"https://example.invalid"}]}"#;
    std::fs::write(&user, original).unwrap();

    let resolved = ConfigService::new(user.clone(), None::<PathBuf>)
        .resolve()
        .expect("settings resolve");
    assert_eq!(resolved.model.as_deref(), Some("claude-haiku-4-5-20251001"));

    assert_eq!(
        std::fs::read_to_string(&user).unwrap(),
        original,
        "a current-version load must not rewrite the file"
    );
}

#[test]
fn migration_keeps_keys_the_settings_struct_does_not_model() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("settings.json");
    std::fs::write(
        &user,
        r#"{"config_version":3,"plugins":{"p":{"options":{"K":"v"}}}}"#,
    )
    .unwrap();

    ConfigService::new(user.clone(), None::<PathBuf>)
        .resolve()
        .expect("settings resolve");

    let migrated = std::fs::read_to_string(&user).unwrap();
    let value: serde_json::Value = serde_json::from_str(&migrated).unwrap();
    assert_eq!(
        value["config_version"].as_u64(),
        Some(u64::from(coderex_config::CURRENT_SCHEMA_VERSION))
    );
    assert_eq!(
        value["plugins"]["p"]["options"]["K"], "v",
        "migration dropped the plugin settings: {migrated}"
    );
}
