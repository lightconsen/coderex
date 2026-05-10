//! Integration commands: /ide, /chrome, /desktop, /mobile, /voice, /vim, /remote-env, /remote-setup.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

thread_local! {
    static TEST_CONFIG_HOME: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_test_config_home(path: std::path::PathBuf) {
    TEST_CONFIG_HOME.with(|h| *h.borrow_mut() = Some(path));
}

fn config_home() -> std::path::PathBuf {
    TEST_CONFIG_HOME.with(|h| {
        h.borrow()
            .clone()
            .unwrap_or_else(|| coderex_config::settings::config_home())
    })
}

/// Read the user settings.json as a JSON object.
fn read_user_settings() -> Option<serde_json::Map<String, serde_json::Value>> {
    let path = config_home().join("settings.json");
    if !path.exists() {
        return None;
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
}

/// Write the user settings.json.
fn write_user_settings(settings: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let home = config_home();
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let path = home.join("settings.json");
    let value = serde_json::Value::Object(settings.clone());
    let json = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}

/// Register integration commands.
pub fn register(registry: &mut CommandRegistry) {
    // /ide — IDE integration
    registry.register(SlashCommand::local(
        "ide",
        "IDE integration connection status",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::success("IDE: Not connected\nUse /ide connect to link your IDE.")
            } else {
                match args[0].as_str() {
                    "connect" => {
                        CommandResult::success(
                            "IDE connection requested.\n(Actual IDE connection is handled by the REPL.)"
                        ).with_state_change(true)
                    }
                    "disconnect" => {
                        CommandResult::success(
                            "IDE disconnection requested.\n(Actual IDE connection is handled by the REPL.)"
                        ).with_state_change(true)
                    }
                    _ => CommandResult::error("Usage: /ide [connect|disconnect]"),
                }
            }
        }),
    ));

    // /chrome — Chrome DevTools integration
    registry.register(SlashCommand::local(
        "chrome",
        "Chrome DevTools integration",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            let connected = std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], 9222)),
                std::time::Duration::from_millis(50),
            ).is_ok();

            if let Some(arg) = args.first() {
                if arg == "--tools" {
                    return CommandResult::success(
                        "Chrome Tools\n───────────\n\
chrome_list_tabs      — List open Chrome tabs\n\
chrome_navigate       — Open a URL in a new tab\n\
chrome_execute_js     — Execute JavaScript in a tab\n\
chrome_screenshot     — Capture a screenshot (base64 PNG)\n\
chrome_get_content    — Extract page HTML as markdown"
                    );
                }
            }

            let status = if connected {
                "Connected\nDebugger listening on port 9222."
            } else {
                "Not connected\n\nStart Chrome with:\n  google-chrome --remote-debugging-port=9222\n  # or\n  /Applications/Google\\ Chrome.app/Contents/MacOS/Google\\ Chrome --remote-debugging-port=9222"
            };
            CommandResult::success(format!(
                "Chrome DevTools\n───────────────\nStatus: {status}\n\nUse /chrome --tools to see available tools."
            ))
        }),
    ));

    // /desktop — desktop app integration
    registry.register(SlashCommand::local(
        "desktop",
        "Desktop app integration",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            let has_desktop = std::env::var("CODEREX_DESKTOP")
                .ok()
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            if has_desktop {
                CommandResult::success("Desktop App: Connected\nRunning with desktop integration.")
            } else {
                CommandResult::success("Desktop App: Not detected\nRunning in terminal mode.")
            }
        }),
    ));

    // /mobile — mobile remote connection
    registry.register(SlashCommand::local(
        "mobile",
        "Mobile remote connection status",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            let has_mobile = std::env::var("CODEREX_MOBILE")
                .ok()
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            if has_mobile {
                CommandResult::success("Mobile: Active\nMobile client connected via Bridge.")
            } else {
                CommandResult::success(
                    "Mobile: No active connections\nUse Bridge to connect mobile clients.",
                )
            }
        }),
    ));

    // /voice — voice mode toggle
    registry.register(SlashCommand::local(
        "voice",
        "Toggle voice mode",
        false,
        false,
        false,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.is_feature_enabled("voice_mode") {
                CommandResult::success("Voice Mode: Toggled")
            } else {
                CommandResult::error(
                    "Voice mode is not available. Enable the voice_mode feature flag.",
                )
            }
        }),
    ));

    // /vim — vim mode toggle
    registry.register(SlashCommand::local(
        "vim",
        "Toggle vim mode",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let new_state = !ctx.vim_mode;
            let mut settings = read_user_settings().unwrap_or_default();
            settings.insert(
                "vim_mode".to_string(),
                serde_json::Value::Bool(new_state),
            );
            let persist_msg = match write_user_settings(&settings) {
                Ok(()) => "Saved to settings.json.".to_string(),
                Err(e) => format!("Failed to save settings: {e}"),
            };
            CommandResult::success(format!(
                "Vim mode: {}\n{persist_msg}",
                if new_state { "enabled" } else { "disabled" }
            ))
            .with_state_change(true)
        }),
    ));

    // /remote-env — view/set remote env vars
    registry.register(SlashCommand::local(
        "remote-env",
        "View or set remote environment variables",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                let mut output = String::from("Environment Variables:\n");
                if ctx.env.is_empty() {
                    output.push_str("  (none set)");
                } else {
                    for (k, v) in &ctx.env {
                        output.push_str(&format!("  {k}={v}\n"));
                    }
                }
                CommandResult::success(output)
            } else {
                // Parse KEY=VALUE from the first argument
                let arg = &args[0];
                let (key, value) = if let Some(eq) = arg.find('=') {
                    (arg[..eq].to_string(), arg[eq + 1..].to_string())
                } else if args.len() >= 2 {
                    // Support: /remote-env KEY VALUE
                    (arg.clone(), args[1..].join(" "))
                } else {
                    return CommandResult::error(
                        "Usage: /remote-env KEY=VALUE or /remote-env KEY VALUE",
                    );
                };

                let mut settings = read_user_settings().unwrap_or_default();
                let mut env_obj = settings
                    .get("env")
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default();
                env_obj.insert(key.clone(), serde_json::Value::String(value.clone()));
                settings.insert("env".to_string(), serde_json::Value::Object(env_obj));

                let persist_msg = match write_user_settings(&settings) {
                    Ok(()) => "Saved to settings.json.".to_string(),
                    Err(e) => format!("Failed to save settings: {e}"),
                };
                CommandResult::success(format!(
                    "Environment variable set: {key}={value}\n{persist_msg}"
                ))
                .with_state_change(true)
            }
        }),
    ));

    // /remote-setup — remote environment setup
    registry.register(SlashCommand::local(
        "remote-setup",
        "Initialize remote environment",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            let bridge_url = std::env::var("CODEREX_BRIDGE_URL").ok();
            let api_key_set = std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
                || std::env::var("OPENAI_API_KEY")
                    .ok()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
            let mut lines = vec!["Remote Setup".to_string(), "───────────".to_string()];
            if api_key_set {
                lines.push("API key: configured".to_string());
            } else {
                lines.push("API key: not set".to_string());
                lines.push("  export ANTHROPIC_API_KEY=...  # or OPENAI_API_KEY".to_string());
            }
            if let Some(url) = bridge_url.filter(|s| !s.is_empty()) {
                lines.push(format!("Bridge URL: {url}"));
                lines.push("Bridge is configured. Use /bridge to check status.".to_string());
            } else {
                lines.push("Bridge: not configured".to_string());
                lines.push("  export CODEREX_BRIDGE_URL=ws://host:port".to_string());
            }
            lines.push(String::new());
            lines.push("Run: coderex serve".to_string());
            lines.push("Use Bridge for full remote session support.".to_string());
            CommandResult::success(lines.join("\n"))
        }),
    ));

    // /statusline-setup — statusline configuration wizard
    registry.register(SlashCommand::local(
        "statusline-setup",
        "Configure statusline preferences",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let format = if ctx.vim_mode {
                "[model] [tokens] [cost] [permissions] [vim]"
            } else {
                "[model] [tokens] [cost] [permissions]"
            };
            let mut settings = read_user_settings().unwrap_or_default();
            let mut statusline = settings
                .get("statusline")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            statusline.insert("format".to_string(), serde_json::Value::String(format.to_string()));
            statusline.insert("theme".to_string(), serde_json::Value::String(ctx.theme.clone()));
            settings.insert("statusline".to_string(), serde_json::Value::Object(statusline));
            let persist_msg = match write_user_settings(&settings) {
                Ok(()) => "Saved to settings.json.".to_string(),
                Err(e) => format!("Failed to save settings: {e}"),
            };
            CommandResult::success(format!(
                "Statusline Setup\n────────────────\nFormat: {format}\nTheme: {}\n\n{persist_msg}",
                ctx.theme,
            ))
            .with_state_change(true)
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
    fn ide_not_connected() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("ide", &ctx(), &[]);
        assert!(result.output.contains("Not connected"));
    }

    #[test]
    fn ide_connect() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("ide", &ctx(), &["connect".into()]);
        assert!(result.output.contains("IDE connection"));
        assert!(result.state_changed);
    }

    #[test]
    fn chrome_not_connected() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("chrome", &ctx(), &[]);
        assert!(result.output.contains("Not connected"));
    }

    #[test]
    fn desktop_not_detected() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("desktop", &ctx(), &[]);
        assert!(result.output.contains("terminal mode"));
    }

    #[test]
    fn mobile_no_connections() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("mobile", &ctx(), &[]);
        assert!(result.output.contains("No active connections"));
    }

    #[test]
    fn voice_feature_gated() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("voice", &ctx(), &[]);
        assert!(result.output.contains("not available"));
    }

    #[test]
    fn vim_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        // Default is false
        let result = reg.execute("vim", &ctx(), &[]);
        assert!(result.output.contains("enabled"));
        assert!(result.state_changed);
        // Verify settings.json was written
        let settings_path = tmp.path().join("settings.json");
        assert!(settings_path.exists());
        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(content.contains("vim_mode"));
        assert!(content.contains("true"));
    }

    #[test]
    fn vim_already_on() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let c = CommandContext {
            vim_mode: true,
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("vim", &c, &[]);
        assert!(result.output.contains("disabled"));
        assert!(result.state_changed);
        // Verify settings.json was written with false
        let settings_path = tmp.path().join("settings.json");
        assert!(settings_path.exists());
        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(content.contains("vim_mode"));
        assert!(content.contains("false"));
    }

    #[test]
    fn remote_env_empty() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("remote-env", &ctx(), &[]);
        assert!(result.output.contains("none set"));
    }

    #[test]
    fn remote_env_with_vars() {
        let mut env = std::collections::HashMap::new();
        env.insert("PATH".into(), "/usr/bin".into());
        let c = CommandContext { env, ..ctx() };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("remote-env", &c, &[]);
        assert!(result.output.contains("PATH"));
    }

    #[test]
    fn remote_env_set_persists() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("remote-env", &ctx(), &["FOO=bar".into()]);
        assert!(result.output.contains("FOO=bar"));
        assert!(result.state_changed);
        // Verify settings.json was written
        let settings_path = tmp.path().join("settings.json");
        assert!(settings_path.exists());
        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(content.contains("FOO"));
        assert!(content.contains("bar"));
    }

    #[test]
    fn remote_setup_instructions() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("remote-setup", &ctx(), &[]);
        assert!(result.output.contains("API key"));
        assert!(result.output.contains("Bridge"));
    }

    #[test]
    fn statusline_setup_opens_wizard() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("statusline-setup", &ctx(), &[]);
        assert!(result.output.contains("Statusline Setup"));
        assert!(result.state_changed);
        // Verify settings.json was written
        let settings_path = tmp.path().join("settings.json");
        assert!(settings_path.exists());
        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(content.contains("statusline"));
        assert!(content.contains("format"));
    }
}
