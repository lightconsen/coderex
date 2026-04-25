//! Integration commands: /ide, /chrome, /desktop, /mobile, /voice, /vim, /remote-env, /remote-setup.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

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
                        CommandResult::success("Connecting to IDE...").with_state_change(true)
                    }
                    "disconnect" => {
                        CommandResult::success("Disconnected from IDE.").with_state_change(true)
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
            CommandResult::success(format!(
                "Vim mode: {}",
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
                CommandResult::success(format!("Setting env: {}", args[0])).with_state_change(true)
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
            CommandResult::success(format!(
                "Statusline Setup\n────────────────\nFormat: {format}\nTheme: {}\n\nConfigure in ~/.coderex/settings.json:\n  \"statusline\": {{\n    \"format\": \"{format}\",\n    \"theme\": \"{}\"\n  }}",
                ctx.theme,
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
        assert!(result.output.contains("Connecting"));
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
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        // Default is false
        let result = reg.execute("vim", &ctx(), &[]);
        assert!(result.output.contains("enabled"));
        assert!(result.state_changed);
    }

    #[test]
    fn vim_already_on() {
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
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("statusline-setup", &ctx(), &[]);
        assert!(result.output.contains("Statusline Setup"));
        assert!(result.state_changed);
    }
}
