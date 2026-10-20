//! Config commands: /config, /model, /permissions.

use crate::registry::{CommandArgs, CommandContext, CommandRegistry, CommandResult, SlashCommand};
use coderex_core::permissions::PermissionMode;
use coderex_core::types::ModelId;
use std::sync::Arc;

thread_local! {
    static TEST_CONFIG_HOME: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_test_config_home(path: std::path::PathBuf) {
    TEST_CONFIG_HOME.with(|h| *h.borrow_mut() = Some(path));
}

fn config_home_for_commands() -> std::path::PathBuf {
    TEST_CONFIG_HOME.with(|h| {
        h.borrow()
            .clone()
            .unwrap_or_else(coderex_config::settings::config_home)
    })
}

/// Read the user settings.json as a JSON object.
fn read_user_settings() -> Option<serde_json::Map<String, serde_json::Value>> {
    let path = config_home_for_commands().join("settings.json");
    if !path.exists() {
        return None;
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
}

/// Write the user settings.json.
fn write_user_settings(
    settings: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let home = config_home_for_commands();
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let path = home.join("settings.json");
    let value = serde_json::Value::Object(settings.clone());
    let json = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}

/// Register config commands.
pub fn register(registry: &mut CommandRegistry) {
    // /config — show/edit config
    registry.register(SlashCommand::local(
        "config",
        "Show or edit configuration",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                // Show current config state
                let mut lines = vec![
                    "Configuration".to_string(),
                    "─────────────".to_string(),
                    format!("Model: {}", ctx.model.0),
                    format!("Permissions: {:?}", ctx.permission_mode),
                    format!("Theme: {}", ctx.theme),
                    format!("Vim: {}", ctx.vim_mode),
                    format!("Working Dir: {}", ctx.cwd),
                    format!("MCP Connected: {}", ctx.mcp_connected),
                ];
                if let Some(settings) = read_user_settings() {
                    lines.push(String::new());
                    lines.push("User settings.json:".to_string());
                    for (k, v) in settings {
                        lines.push(format!("  {k}: {v}"));
                    }
                }
                CommandResult::success(lines.join("\n"))
            } else if args.len() >= 3 && args[0] == "set" {
                let key = &args[1];
                let value_str = args[2..].join(" ");
                // Parse the value
                let value = value_str.parse::<i64>().map_or_else(
                    |_| {
                        value_str.parse::<f64>().map_or_else(
                            |_| match value_str.as_str() {
                                "true" => serde_json::Value::Bool(true),
                                "false" => serde_json::Value::Bool(false),
                                "null" => serde_json::Value::Null,
                                _ => serde_json::Value::String(value_str.clone()),
                            },
                            |v| {
                                serde_json::Number::from_f64(v).map_or_else(
                                    || serde_json::Value::String(value_str.clone()),
                                    serde_json::Value::Number,
                                )
                            },
                        )
                    },
                    |v| serde_json::Value::Number(v.into()),
                );
                let mut settings = read_user_settings().unwrap_or_default();
                settings.insert(key.clone(), value);
                match write_user_settings(&settings) {
                    Ok(()) => CommandResult::success(format!("Set {key} in settings.json"))
                        .with_state_change(true),
                    Err(e) => CommandResult::error(format!("Failed to write settings: {e}")),
                }
            } else if args.len() == 2 && args[0] == "get" {
                let key = &args[1];
                let settings = read_user_settings().unwrap_or_default();
                settings.get(key).map_or_else(
                    || CommandResult::error(format!("Key '{key}' not found in settings")),
                    |v| CommandResult::success(format!("{key}: {v}")),
                )
            } else if args.len() == 1 && args[0] == "list" {
                let settings = read_user_settings().unwrap_or_default();
                if settings.is_empty() {
                    CommandResult::success("No user settings configured.\nUse /config set <key> <value> to add settings.\nKeys: model, permission_mode, max_budget_usd, telemetry_enabled, api_key")
                } else {
                    let lines: Vec<String> = settings
                        .iter()
                        .map(|(k, v)| format!("  {k}: {v}"))
                        .collect();
                    CommandResult::success(format!(
                        "User settings ({} entries):\n{}",
                        lines.len(),
                        lines.join("\n")
                    ))
                }
            } else {
                CommandResult::error("Usage: /config [set <key> <value> | get <key> | list]")
            }
        }),
    ));

    // /model — switch model
    registry.register(
        SlashCommand::local(
            "model",
            "Switch AI model",
            true,
            true,
            true,
            Arc::new(|ctx: &CommandContext, args: &[String]| {
                if args.is_empty() {
                    CommandResult::success(format!(
                        "Current model: {}\nUsage: /model <opus|sonnet|haiku|custom-model-id>",
                        ctx.model.0
                    ))
                } else {
                    let model_name = match args[0].to_lowercase().as_str() {
                        "opus" => ModelId::opus().0,
                        "sonnet" => ModelId::sonnet().0,
                        "haiku" => ModelId::haiku().0,
                        other => other.to_string(),
                    };
                    // Also persist to settings.json so the change survives restart
                    let mut settings = read_user_settings().unwrap_or_default();
                    settings.insert("model".to_string(), serde_json::Value::String(model_name.clone()));
                    let _ = write_user_settings(&settings);
                    CommandResult::success(format!(
                        "Model switched to: {model_name}\nSaved to settings.json. Use in the REPL for immediate effect.",
                    )).with_state_change(true)
                }
            }),
        )
        .with_args(CommandArgs::AtMost(1)),
    );

    // /permissions — view/set permission mode
    registry.register(SlashCommand::local(
        "permissions",
        "View or set permission mode",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::success(format!(
                    "Current permission mode: {:?}\nAvailable: default, acceptedits, bypasspermissions, dontask, plan, auto",
                    ctx.permission_mode,
                ))
            } else {
                let mode = match args[0].to_lowercase().as_str() {
                    "default" => PermissionMode::Default,
                    "acceptedits" | "accept_edits" => PermissionMode::AcceptEdits,
                    "bypasspermissions" | "bypass" | "bypass_permissions" => PermissionMode::BypassPermissions,
                    "dontask" | "dont_ask" => PermissionMode::DontAsk,
                    "plan" | "planmode" | "plan_mode" => PermissionMode::Plan,
                    "auto" => PermissionMode::Auto,
                    _ => {
                        return CommandResult::error(
                            "Unknown mode. Available: default, acceptedits, bypasspermissions, dontask, plan, auto"
                        );
                    }
                };
                // Also persist to settings.json so the change survives restart
                let mode_str = format!("{mode:?}").to_lowercase();
                let mut settings = read_user_settings().unwrap_or_default();
                settings.insert("permission_mode".to_string(), serde_json::Value::String(mode_str));
                let _ = write_user_settings(&settings);
                CommandResult::success(format!(
                    "Permission mode set to: {mode:?}\nSaved to settings.json. Use in the REPL for immediate effect.",
                )).with_state_change(true)
            }
        }),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::types::ModelId;

    fn ctx() -> CommandContext {
        CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
    }

    #[test]
    fn config_shows_current_state() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("config", &ctx(), &[]);
        assert!(result.output.contains("Model"));
        assert!(result.output.contains("Permissions"));
    }

    #[test]
    fn config_set_writes_value() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute(
            "config",
            &ctx(),
            &["set".into(), "model".into(), "opus".into()],
        );
        assert!(result.output.contains("Set model"));
        assert!(result.state_changed);
        // Verify file was written
        let settings_path = tmp.path().join("settings.json");
        assert!(settings_path.exists());
        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(content.contains("model"));
        assert!(content.contains("opus"));
    }

    #[test]
    fn config_get_reads_value() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        // Pre-write a settings file
        let settings = serde_json::json!({"model": "sonnet"});
        std::fs::write(tmp.path().join("settings.json"), settings.to_string()).unwrap();
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("config", &ctx(), &["get".into(), "model".into()]);
        assert!(result.output.contains("sonnet"));
    }

    #[test]
    fn config_list_shows_entries() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let settings = serde_json::json!({"model": "haiku", "telemetry_enabled": false});
        std::fs::write(tmp.path().join("settings.json"), settings.to_string()).unwrap();
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("config", &ctx(), &["list".into()]);
        assert!(result.output.contains("haiku"));
    }

    #[test]
    fn model_switch_shows_current() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("model", &ctx(), &[]);
        assert!(result.output.contains("Current model"));
    }

    #[test]
    fn model_switch_to_opus() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("model", &ctx(), &["opus".into()]);
        assert!(result.output.contains("claude-opus"));
        assert!(result.state_changed);
        // Verify settings.json was written
        let settings_path = tmp.path().join("settings.json");
        assert!(settings_path.exists());
    }

    #[test]
    fn model_switch_to_haiku() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("model", &ctx(), &["haiku".into()]);
        assert!(result.output.contains("haiku"));
    }

    #[test]
    fn model_switch_unknown() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("model", &ctx(), &["unknown-model".into()]);
        assert!(result.output.contains("unknown-model"));
    }

    #[test]
    fn permissions_shows_current() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("permissions", &ctx(), &[]);
        assert!(result.output.contains("permission mode"));
    }

    #[test]
    fn permissions_set_bypass() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("permissions", &ctx(), &["bypass".into()]);
        assert!(result.output.contains("BypassPermissions"));
        assert!(result.state_changed);
        // Verify settings.json was written
        let settings_path = tmp.path().join("settings.json");
        assert!(settings_path.exists());
    }

    #[test]
    fn permissions_set_plan() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("permissions", &ctx(), &["plan".into()]);
        assert!(result.output.contains("Plan"));
    }

    #[test]
    fn permissions_set_unknown_mode() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("permissions", &ctx(), &["foobar".into()]);
        assert!(result.output.contains("Error"));
    }
}
