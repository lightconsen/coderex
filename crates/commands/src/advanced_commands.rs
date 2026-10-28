//! Advanced commands: /bridge, /bridge-kick, /teleport, /ultraplan, /insights, /thinkback, /thinkback-play, /statusline.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::fmt::Write as _;
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
            let status = if bridge_url.as_ref().is_some_and(|s| !s.is_empty()) {
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
            if bridge_url.as_ref().is_some_and(|s| !s.is_empty()) {
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
                // Session-scoped, like /advisor: there is no cloud planning
                // backend, so the mode is a prompt instruction. It is not
                // persisted — `ultraplan` is not a settings key, and writing
                // one would make the next load reject the whole file.
                CommandResult::success(
                    "Ultra Plan Mode activated for this session.\n\
                     (Multi-phase planning instructions added to the prompt)",
                )
                .with_context(
                    "Ultra Plan mode: before changing files, produce a multi-phase plan \
                     (goals, constraints, ordered steps, verification for each step) and \
                     keep it updated as work progresses.",
                )
                .with_append_system_prompt(
                    "You are in Ultra Plan mode. Plan in explicit phases, call out \
                     assumptions and verification steps, and prefer a short plan over \
                     immediate edits.",
                )
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
                        let _ = write!(report, "\n• {} — {}", finding.title, finding.detail);
                    }
                    for unreadable in &outcome.unreadable {
                        let _ = write!(report, "\n(could not read {unreadable})");
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
                            let _ = write!(
                                output,
                                "Session {}: {} messages\n\n",
                                ctx.session_id, total_msgs
                            );
                            for line in lines.iter().take(10) {
                                if let Ok(msg) = serde_json::from_str::<serde_json::Value>(line)
                                    && let Some(role) = msg.get("role").and_then(|r| r.as_str())
                                {
                                    let preview =
                                        msg.get("content").and_then(|c| c.as_str()).map_or_else(
                                            || "(non-text)".to_string(),
                                            |s| {
                                                let s = s.trim();
                                                if s.len() > 60 {
                                                    format!("{}...", &s[..57])
                                                } else {
                                                    s.to_string()
                                                }
                                            },
                                        );
                                    let role_label = match role {
                                        "user" => "User",
                                        "assistant" => "Assistant",
                                        "tool" => "Tool",
                                        "system" => "System",
                                        _ => role,
                                    };
                                    let _ = writeln!(output, "  [{role_label}] {preview}");
                                }
                            }
                            if total_msgs > 10 {
                                let _ =
                                    writeln!(output, "  ... and {} more messages", total_msgs - 10);
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
                if transcript_dir.exists()
                    && let Ok(entries) = std::fs::read_dir(&transcript_dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().is_some_and(|e| e == "jsonl") {
                                found += 1;
                            }
                        }
                    }
                if found == 0 {
                    return CommandResult::success(
                        "Thinkback Play\n─────────────\nNo transcripts found; there is nothing to replay.",
                    );
                }
                // The play step is a model turn built from a local transcript
                // review, not a hidden REPL branch: the command hands the
                // session a prompt, the way /advisor hands it a profile.
                CommandResult::success(format!(
                    "Thinkback Play\n─────────────\nFound {found} transcript file(s).\n\
                     Reviewing them in the next turn."
                ))
                .with_context(format!(
                    "Thinkback Play: review the {found} transcript file(s) under {} \
                     and propose the concrete next actions they imply. Read only what you \
                     need before steering the work.",
                    transcript_dir.display()
                ))
                .with_append_system_prompt(
                    "Thinkback Play: you have just reviewed earlier sessions. Summarise \
                     what was learned and continue the work from those findings rather than \
                     starting over.",
                )
                .with_state_change(true)
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
        let home = tempfile::tempdir().unwrap();
        // A settings file this command must not touch: `ultraplan` is not a
        // known settings key, and writing it would make the next load reject
        // the whole file as an unknown field.
        let settings_path = home.path().join("settings.json");
        std::fs::write(&settings_path, r#"{"config_version": 4}"#).unwrap();
        let before = std::fs::read_to_string(&settings_path).unwrap();

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
        assert!(
            result.append_system_prompt.is_some(),
            "the mode must reach the prompt"
        );
        assert_eq!(
            std::fs::read_to_string(&settings_path).unwrap(),
            before,
            "the command rewrote settings.json"
        );
    }

    #[test]
    fn thinkback_play_hands_the_session_a_prompt() {
        let workspace = tempfile::tempdir().unwrap();
        let transcripts = workspace.path().join(".coderex/transcripts");
        std::fs::create_dir_all(&transcripts).unwrap();
        std::fs::write(transcripts.join("sess_1.jsonl"), "{}\n").unwrap();

        let mut flags = std::collections::HashMap::new();
        flags.insert("thinkback".into(), true);
        let c = CommandContext {
            feature_flags: flags,
            cwd: workspace.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("thinkback-play", &c, &[]);
        assert!(result.output.contains("1 transcript"), "{}", result.output);
        assert!(result.state_changed);
        let context = result.extra_context.expect("review context");
        assert!(context.contains("Thinkback Play"), "{context}");
        assert!(result.append_system_prompt.is_some());
    }

    #[test]
    fn thinkback_play_with_no_transcripts_does_not_promise_a_turn() {
        let workspace = tempfile::tempdir().unwrap();
        let mut flags = std::collections::HashMap::new();
        flags.insert("thinkback".into(), true);
        let c = CommandContext {
            feature_flags: flags,
            cwd: workspace.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("thinkback-play", &c, &[]);
        assert!(
            result.output.contains("nothing to replay"),
            "{}",
            result.output
        );
        assert!(result.extra_context.is_none());
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
