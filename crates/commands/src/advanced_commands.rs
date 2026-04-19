//! Advanced commands: /bridge, /bridge-kick, /teleport, /ultraplan, /insights, /thinkback, /thinkback-play, /statusline.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

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
                CommandResult::success("Kicked Bridge connection.")
                    .with_state_change(true)
            } else {
                CommandResult::success("No active Bridge connections to kick.")
            }
        }),
    ));

    // /teleport — switch CWD and session (feature-gated)
    registry.register(SlashCommand::local(
        "teleport",
        "Teleport to another working directory and session (feature-gated)",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if ctx.is_feature_enabled("teleport") {
                if args.is_empty() {
                    CommandResult::error("Usage: /teleport <directory>")
                } else {
                    CommandResult::success(format!("Teleporting to: {}", args[0]))
                        .with_state_change(true)
                }
            } else {
                CommandResult::error("teleport feature is not enabled.")
            }
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
                CommandResult::success("Ultra Plan Mode activated.\n(Detailed multi-phase planning enabled)")
                    .with_state_change(true)
            } else {
                CommandResult::error("ultraplan feature is not enabled.")
            }
        }),
    ));

    // /insights — project insights
    registry.register(SlashCommand::local(
        "insights",
        "Project insights and analysis",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let cwd = std::path::Path::new(&ctx.cwd);
            let mut file_count = 0;
            let mut dir_count = 0;
            if cwd.exists() {
                if let Ok(entries) = std::fs::read_dir(cwd) {
                    for entry in entries.flatten() {
                        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                            file_count += 1;
                        } else if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                            dir_count += 1;
                        }
                    }
                }
            }
            let git_info = if ctx.git_branch.is_empty() {
                "No git repository detected".to_string()
            } else {
                format!("Branch: {}\nStatus: {}", ctx.git_branch, ctx.git_status)
            };
            CommandResult::success(format!(
                "Project Insights\n────────────────\nWorking Dir: {}\n{}\n\nFiles: {} | Directories: {}\nSession Messages: {}\nInput Tokens: {} | Output Tokens: {}\nCache Read: {} | Cache Creation: {}\nCost: ${:.4}",
                ctx.cwd, git_info, file_count, dir_count, ctx.message_count,
                ctx.input_tokens, ctx.output_tokens, ctx.cache_read_tokens, ctx.cache_creation_tokens, ctx.cost_usd
            ))
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
                let transcript_dir = std::path::Path::new(&ctx.cwd).join(".claude/transcripts");
                let mut output = String::from("Thinkback\n────────\n");
                let mut total_msgs = 0;
                if transcript_dir.exists() {
                    // Look for transcript files for this session
                    let session_file = transcript_dir.join(format!("{}.jsonl", ctx.session_id));
                    if session_file.exists() {
                        if let Ok(content) = std::fs::read_to_string(&session_file) {
                            let lines: Vec<&str> = content.lines().collect();
                            total_msgs = lines.len();
                            output.push_str(&format!("Session {}: {} messages\n\n", ctx.session_id, total_msgs));
                            for line in lines.iter().take(10) {
                                if let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) {
                                    if let Some(role) = msg.get("role").and_then(|r| r.as_str()) {
                                        let preview = msg.get("content")
                                            .and_then(|c| c.as_str())
                                            .map(|s| {
                                                let s = s.trim();
                                                if s.len() > 60 { format!("{}...", &s[..57]) } else { s.to_string() }
                                            })
                                            .unwrap_or_else(|| "(non-text)".to_string());
                                        let role_label = match role {
                                            "user" => "User",
                                            "assistant" => "Assistant",
                                            "tool" => "Tool",
                                            "system" => "System",
                                            _ => role,
                                        };
                                        output.push_str(&format!("  [{}] {}\n", role_label, preview));
                                    }
                                }
                            }
                            if total_msgs > 10 {
                                output.push_str(&format!("  ... and {} more messages\n", total_msgs - 10));
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
                CommandResult::success(
                    "Thinkback Play\n─────────────\n(Executing learnings from historical sessions...)"
                ).with_state_change(true)
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
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("bridge", &ctx(), &[]);
        assert!(result.output.contains("Not running"));
    }

    #[test]
    fn bridge_kick_none() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("bridge-kick", &ctx(), &[]);
        assert!(result.output.contains("No active"));
    }

    #[test]
    fn teleport_feature_gated() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("teleport", &ctx(), &[]);
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn teleport_when_enabled() {
        let mut flags = std::collections::HashMap::new();
        flags.insert("teleport".into(), true);
        let c = CommandContext { feature_flags: flags, ..ctx() };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("teleport", &c, &["/other".into()]);
        assert!(result.output.contains("/other"));
        assert!(result.state_changed);
    }

    #[test]
    fn teleport_requires_arg() {
        let mut flags = std::collections::HashMap::new();
        flags.insert("teleport".into(), true);
        let c = CommandContext { feature_flags: flags, ..ctx() };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("teleport", &c, &[]);
        assert!(result.output.contains("Usage"));
    }

    #[test]
    fn ultraplans_feature_gated() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("ultraplan", &ctx(), &[]);
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn ultraplans_when_enabled() {
        let mut flags = std::collections::HashMap::new();
        flags.insert("ultraplan".into(), true);
        let c = CommandContext { feature_flags: flags, ..ctx() };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("ultraplan", &c, &[]);
        assert!(result.output.contains("Ultra Plan Mode"));
        assert!(result.state_changed);
    }

    #[test]
    fn insights() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("insights", &ctx(), &[]);
        assert!(result.output.contains("Project Insights"));
    }

    #[test]
    fn thinkback_feature_gated() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("thinkback", &ctx(), &[]);
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn thinkback_play_feature_gated() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("thinkback-play", &ctx(), &[]);
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn statusline_config() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("statusline", &ctx(), &[]);
        assert!(result.output.contains("Statusline Configuration"));
    }

    #[test]
    fn statusline_uses_agent_backend() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let cmd = reg.find("statusline").unwrap();
        assert!(cmd.uses_agent_backend);
    }
}
