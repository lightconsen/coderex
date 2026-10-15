//! Advanced commands: /bridge, /bridge-kick, /teleport, /ultraplan, /insights, /thinkback, /thinkback-play, /statusline.

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
fn write_user_settings(
    settings: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let home = config_home();
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    let path = home.join("settings.json");
    let value = serde_json::Value::Object(settings.clone());
    let json = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(())
}

/// Register advanced commands.
pub fn register(registry: &mut CommandRegistry) {
    // /bridge — Bridge status/management
    registry.register(SlashCommand::local(
        "bridge",
        "Bridge status and management",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            let bridge_url = std::env::var("CODEREX_BRIDGE_URL").ok();
            let status = if bridge_url.as_ref().map(|s| !s.is_empty()).unwrap_or(false) {
                format!(
                    "Status: Configured\nURL: {}\nNo remote environments connected yet.",
                    bridge_url.unwrap()
                )
            } else {
                "Status: Not running\nNo remote environments connected.\n\nSet CODEREX_BRIDGE_URL to configure Bridge.".to_string()
            };
            CommandResult::success(format!("Bridge\n──────\n{status}"))
        }),
    ));

    // /bridge-kick — kick remote Bridge connection
    registry.register(SlashCommand::local(
        "bridge-kick",
        "Kick a remote Bridge connection",
        true,
        true,
        false,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            let bridge_url = std::env::var("CODEREX_BRIDGE_URL").ok();
            if bridge_url.as_ref().map(|s| !s.is_empty()).unwrap_or(false) {
                CommandResult::success(
                    "Bridge kick requested.\n(Actual connection management is handled by the REPL or Bridge daemon.)"
                ).with_state_change(true)
            } else {
                CommandResult::success("No active Bridge connections to kick.")
            }
        }),
    ));

    // /teleport — handled directly by the REPL (built-in command).
    // This registry stub exists for non-REPL consumers.
    registry.register(SlashCommand::local(
        "teleport",
        "Teleport to another working directory and session (REPL built-in)",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success("Teleport is handled by the REPL. Use /teleport <directory> | --share | --join <token> | --list | --revoke <token>.")
        }),
    ));

    // /ultraplan — super plan mode (feature-gated)
    registry.register(SlashCommand::local(
        "ultraplan",
        "Ultra plan mode for complex projects (feature-gated)",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.is_feature_enabled("ultraplan") {
                let mut settings = read_user_settings().unwrap_or_default();
                settings.insert(
                    "ultraplan".to_string(),
                    serde_json::Value::Bool(true),
                );
                let persist_msg = match write_user_settings(&settings) {
                    Ok(()) => "Saved to settings.json.".to_string(),
                    Err(e) => format!("Failed to save settings: {e}"),
                };
                CommandResult::success(format!(
                    "Ultra Plan Mode activated.\n(Detailed multi-phase planning enabled)\n{persist_msg}",
                ))
                .with_state_change(true)
            } else {
                CommandResult::error("ultraplan feature is not enabled.")
            }
        }),
    ));

    // /insights — project insights
    registry.register(SlashCommand::local(
        "insights",
        "Analyze your sessions and write a report",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            let transcripts =
                coderex_insights::workspace_transcripts(std::path::Path::new(&ctx.cwd));
            let root = args
                .first()
                .filter(|arg| !arg.starts_with('-'))
                .map_or_else(coderex_insights::default_root, std::path::PathBuf::from);

            match coderex_insights::run(&[transcripts], root) {
                Ok(outcome) => {
                    let mut report = format!(
                        "{}\n\n{}",
                        outcome.summary(),
                        outcome.report.stable.display()
                    );
                    for finding in outcome.findings.iter().take(5) {
                        report.push_str(&format!("\n• {} — {}", finding.title, finding.detail));
                    }
                    for unreadable in &outcome.unreadable {
                        report.push_str(&format!("\n(could not read {unreadable})"));
                    }
                    CommandResult::success(report)
                }
                Err(error) => CommandResult::error(format!("could not write the report: {error}")),
            }
        }),
    ));

    // /thinkback — review historical sessions (feature-gated)
    registry.register(SlashCommand::local(
        "thinkback",
        "Review historical sessions (feature-gated)",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.is_feature_enabled("thinkback") {
                let transcript_dir = std::path::Path::new(&ctx.cwd).join(".coderex/transcripts");
                let mut output = String::from("Thinkback\n────────\n");
                let total_msgs;
                if transcript_dir.exists() {
                    // Look for transcript files for this session
                    let session_file = transcript_dir.join(format!("{}.jsonl", ctx.session_id));
                    if session_file.exists() {
                        if let Ok(content) = std::fs::read_to_string(&session_file) {
                            let lines: Vec<&str> = content.lines().collect();
                            total_msgs = lines.len();
                            let _total = total_msgs;
                            output.push_str(&format!(
                                "Session {}: {} messages\n\n",
                                ctx.session_id, total_msgs
                            ));
                            for line in lines.iter().take(10) {
                                if let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) {
                                    if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
                                        let preview = msg
                                            .get("content")
                                            .and_then(|c| c.as_str())
                                            .map(|s| {
                                                let s = s.trim();
                                                if s.len() > 60 {
                                                    format!("{}...", &s[..57])
                                                } else {
                                                    s.to_string()
                                                }
                                            })
                                            .unwrap_or_else(|| "(non-text)".to_string());
                                        let role_label = match role {
                                            "user" => "User",
                                            "assistant" => "Assistant",
                                            "tool" => "Tool",
                                            "system" => "System",
                                            _ => role,
                                        };
                                        output
                                            .push_str(&format!("  [{}] {}\n", role_label, preview));
                                    }
                                }
                            }
                            if total_msgs > 10 {
                                output.push_str(&format!(
                                    "  ... and {} more messages\n",
                                    total_msgs - 10
                                ));
                            }
                        }
                    } else {
                        output.push_str("No transcript file found for this session.\n");
                    }
                } else {
                    output.push_str("No transcript directory found.\n");
                }
                output.push_str("\nUse /thinkback-play to execute learnings.");
                CommandResult::success(output)
            } else {
                CommandResult::error("thinkback feature is not enabled.")
            }
        }),
    ));

    // /thinkback-play — review and execute (feature-gated)
    registry.register(SlashCommand::local(
        "thinkback-play",
        "Review historical sessions and execute learnings (feature-gated)",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.is_feature_enabled("thinkback") {
                let transcript_dir = std::path::Path::new(&ctx.cwd).join(".coderex/transcripts");
                let mut found = 0;
                if transcript_dir.exists() {
                    if let Ok(entries) = std::fs::read_dir(&transcript_dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().is_some_and(|e| e == "jsonl") {
                                found += 1;
                            }
                        }
                    }
                }
                CommandResult::success(format!(
                    "Thinkback Play\n─────────────\nFound {found} transcript file(s).\nUse /thinkback to review before executing.\n(Execution is handled by the REPL.)"
                )).with_state_change(true)
            } else {
                CommandResult::error("thinkback feature is not enabled.")
            }
        }),
    ));

    // /statusline — statusline custom config (agent-backed)
    registry.register(SlashCommand::local(
        "statusline",
        "Configure the statusline",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(
                "Statusline Configuration\n──────────────────────\nDefault format: [model] [tokens] [cost] [permissions]\n\nCustomize in settings.json: statusline.format"
            )
        }),
    ).with_agent_backend());
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::types::ModelId;

    fn ctx() -> CommandContext {
        CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
    }

    #[test]
    fn bridge_status() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("bridge", &ctx(), &[]);
        assert!(result.output.contains("Not running"));
    }

    #[test]
    fn bridge_kick_none() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("bridge-kick", &ctx(), &[]);
        assert!(result.output.contains("No active"));
    }

    #[test]
    fn teleport_registry_stub() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("teleport", &ctx(), &[]);
        assert!(result.output.contains("handled by the REPL"));
    }

    #[test]
    fn ultraplans_feature_gated() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("ultraplan", &ctx(), &[]);
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn ultraplans_when_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        set_test_config_home(tmp.path().to_path_buf());
        let mut flags = std::collections::HashMap::new();
        flags.insert("ultraplan".into(), true);
        let c = CommandContext {
            feature_flags: flags,
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("ultraplan", &c, &[]);
        assert!(result.output.contains("Ultra Plan Mode"));
        assert!(result.state_changed);
        // Verify settings.json was written
        let settings_path = tmp.path().join("settings.json");
        assert!(settings_path.exists());
        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(content.contains("ultraplan"));
        assert!(content.contains("true"));
    }

    #[test]
    fn insights_writes_a_report_for_the_workspace() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };

        let workspace = tempfile::tempdir().unwrap();
        let out_dir = tempfile::tempdir().unwrap();
        let mut context = ctx();
        context.cwd = workspace.path().to_string_lossy().to_string();

        // A workspace with no transcripts is a sentence, not a failure.
        let result = reg.execute(
            "insights",
            &context,
            &[out_dir.path().to_string_lossy().to_string()],
        );
        assert!(result.output.contains("No sessions"), "{}", result.output);

        // With one session written, the report is produced and named.
        let transcripts = coderex_insights::workspace_transcripts(workspace.path());
        std::fs::create_dir_all(&transcripts).unwrap();
        std::fs::write(
            transcripts.join("sess_1.jsonl"),
            serde_json::to_string(&coderex_core::types::Message::user("fix the parser")).unwrap(),
        )
        .unwrap();

        let result = reg.execute(
            "insights",
            &context,
            &[out_dir.path().to_string_lossy().to_string()],
        );
        assert!(result.output.contains("1 session(s)"), "{}", result.output);
        assert!(result.output.contains("report.html"), "{}", result.output);
        assert!(out_dir.path().join("report.html").is_file());
        assert!(out_dir.path().join("facets").join("sess_1.json").is_file());
    }

    #[test]
    fn thinkback_feature_gated() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("thinkback", &ctx(), &[]);
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn thinkback_play_feature_gated() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("thinkback-play", &ctx(), &[]);
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn statusline_config() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("statusline", &ctx(), &[]);
        assert!(result.output.contains("Statusline Configuration"));
    }

    #[test]
    fn statusline_uses_agent_backend() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let cmd = reg.find("statusline").unwrap();
        assert!(cmd.uses_agent_backend);
    }
}
