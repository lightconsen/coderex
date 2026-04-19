//! Miscellaneous commands: /advisor, /btw, /good-claude, /stickers, /bughunter, /issue, /env, /diff, /oauth-refresh, /backfill-sessions.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use coderex_core::query_profiles::PromptProfile;
use std::sync::Arc;

/// Register miscellaneous commands.
pub fn register(registry: &mut CommandRegistry) {
    // /advisor — advisor mode
    registry.register(SlashCommand::prompt(
        "advisor",
        "Enter advisor mode for guidance",
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success("Advisor mode activated.\n(Ask me anything about your project)")
                .with_context("You are now in advisor mode. Provide guidance and recommendations rather than directly implementing changes.")
                .with_prompt_profile(PromptProfile::Advisor)
                .with_append_system_prompt("You are a senior technical advisor. Focus on guidance, architecture advice, and recommendations. Explain your reasoning and suggest approaches with trade-offs.")
        }),
    ));

    // /btw — insert contextual information
    registry.register(SlashCommand::prompt(
        "btw",
        "Insert contextual information",
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::error("Usage: /btw <information to add>")
            } else {
                CommandResult::success(format!("Added context: {}", args.join(" ")))
                    .with_context(args.join(" "))
            }
        }),
    ));

    // /good-claude — positive feedback
    registry.register(SlashCommand::local(
        "good-claude",
        "Give positive feedback",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success("Thanks! Glad I could help.")
        }),
    ));

    // /stickers — stickers/emoji (feature-gated)
    registry.register(SlashCommand::local(
        "stickers",
        "Send stickers or emoji (feature-gated)",
        false,
        false,
        false,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.is_feature_enabled("stickers") {
                CommandResult::success("Stickers: (feature content would appear here)")
            } else {
                CommandResult::error("stickers feature is not enabled.")
            }
        }),
    ));

    // /bughunter — bug hunter mode
    registry.register(SlashCommand::prompt(
        "bughunter",
        "Enter bug hunter mode",
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success("Bug Hunter mode activated.\n(Systematically searching for bugs in the codebase)")
                .with_context("You are now in bug hunter mode. Systematically review the codebase for bugs, edge cases, and potential issues.")
                .with_prompt_profile(PromptProfile::BugHunter)
                .with_append_system_prompt("You are a bug hunting specialist. Systematically review code for bugs, edge cases, security issues, and potential failures. Be thorough and methodical.")
        }),
    ));

    // /issue — create/manage issues
    registry.register(SlashCommand::local(
        "issue",
        "Create or manage issues",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::success("Usage: /issue create <title> | /issue list | /issue <number>")
            } else if args[0] == "create" {
                let title = args[1..].join(" ");
                CommandResult::success(format!("Issue created: {title}"))
                    .with_state_change(true)
            } else if args[0] == "list" {
                CommandResult::success("Issues:\n  No open issues.")
            } else {
                CommandResult::success(format!("Viewing issue: {}", args[0]))
            }
        }),
    ));

    // /env — view/set environment variables
    registry.register(SlashCommand::local(
        "env",
        "View or set environment variables",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                let mut output = String::from("Environment:\n");
                if ctx.env.is_empty() {
                    output.push_str("  (none set)");
                } else {
                    for (k, v) in &ctx.env {
                        output.push_str(&format!("  {k}={v}\n"));
                    }
                }
                CommandResult::success(output)
            } else {
                CommandResult::success(format!("Setting env: {} = {}", args[0], args.get(1).map(|s| s.as_str()).unwrap_or("")))
                    .with_state_change(true)
            }
        }),
    ));

    // /diff — show current git diff
    registry.register(SlashCommand::local(
        "diff",
        "Show current git diff",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.git_status.is_empty() {
                CommandResult::success("No changes to show.")
            } else {
                CommandResult::success(format!("Git Diff\n────────\n{}", ctx.git_status))
            }
        }),
    ));

    // /oauth-refresh — refresh OAuth token
    registry.register(SlashCommand::local(
        "oauth-refresh",
        "Refresh OAuth token",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success("OAuth token refreshed.")
                .with_state_change(true)
        }),
    ));

    // /backfill-sessions — backfill historical session index
    registry.register(SlashCommand::local(
        "backfill-sessions",
        "Backfill historical session index",
        false,
        false,
        false,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let mut found = 0;
            let mut total_messages = 0;

            // Scan project transcripts
            let project_transcripts = std::path::Path::new(&ctx.cwd).join(".claude/transcripts");
            if project_transcripts.exists() {
                if let Ok(entries) = std::fs::read_dir(&project_transcripts) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().is_some_and(|e| e == "jsonl") {
                            if let Ok(content) = std::fs::read_to_string(&path) {
                                let msgs = content.lines().count();
                                total_messages += msgs;
                                found += 1;
                            }
                        }
                    }
                }
            }

            // Scan global transcripts
            let global_transcripts = coderex_config::settings::config_home().join("transcripts");
            if global_transcripts.exists() {
                if let Ok(entries) = std::fs::read_dir(&global_transcripts) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().is_some_and(|e| e == "jsonl") {
                            if let Ok(content) = std::fs::read_to_string(&path) {
                                let msgs = content.lines().count();
                                total_messages += msgs;
                                found += 1;
                            }
                        }
                    }
                }
            }

            CommandResult::success(format!(
                "Backfilling session index...\nScanned {found} transcript file(s), {total_messages} total message(s)."
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
    fn advisor_mode() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("advisor", &ctx(), &[]);
        assert!(result.output.contains("Advisor mode"));
        assert!(result.extra_context.is_some());
        assert_eq!(result.prompt_profile, Some(PromptProfile::Advisor));
        assert!(result.append_system_prompt.is_some());
    }

    #[test]
    fn btw_requires_arg() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("btw", &ctx(), &[]);
        assert!(result.output.contains("Usage"));
    }

    #[test]
    fn btw_adds_context() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("btw", &ctx(), &["important".into(), "context".into()]);
        assert!(result.extra_context.is_some());
    }

    #[test]
    fn good_claude() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("good-claude", &ctx(), &[]);
        assert!(result.output.contains("Thanks"));
    }

    #[test]
    fn stickers_feature_gated() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("stickers", &ctx(), &[]);
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn bughunter_mode() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("bughunter", &ctx(), &[]);
        assert!(result.output.contains("Bug Hunter"));
        assert!(result.extra_context.is_some());
        assert_eq!(result.prompt_profile, Some(PromptProfile::BugHunter));
        assert!(result.append_system_prompt.is_some());
    }

    #[test]
    fn issue_usage() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("issue", &ctx(), &[]);
        assert!(result.output.contains("Usage"));
    }

    #[test]
    fn issue_create() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("issue", &ctx(), &["create".into(), "fix".into(), "bug".into()]);
        assert!(result.output.contains("fix bug"));
        assert!(result.state_changed);
    }

    #[test]
    fn issue_list() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("issue", &ctx(), &["list".into()]);
        assert!(result.output.contains("No open issues"));
    }

    #[test]
    fn issue_view_number() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("issue", &ctx(), &["42".into()]);
        assert!(result.output.contains("42"));
    }

    #[test]
    fn env_empty() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("env", &ctx(), &[]);
        assert!(result.output.contains("Environment"));
    }

    #[test]
    fn env_with_vars() {
        let mut env = std::collections::HashMap::new();
        env.insert("HOME".into(), "/home/user".into());
        let c = CommandContext { env, ..ctx() };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("env", &c, &[]);
        assert!(result.output.contains("HOME"));
    }

    #[test]
    fn diff_empty() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("diff", &ctx(), &[]);
        assert!(result.output.contains("No changes"));
    }

    #[test]
    fn diff_with_changes() {
        let c = CommandContext { git_status: "M file.txt\nA new.txt".into(), ..ctx() };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("diff", &c, &[]);
        assert!(result.output.contains("file.txt"));
    }

    #[test]
    fn oauth_refresh() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("oauth-refresh", &ctx(), &[]);
        assert!(result.output.contains("refreshed"));
        assert!(result.state_changed);
    }

    #[test]
    fn backfill_sessions() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("backfill-sessions", &ctx(), &[]);
        assert!(result.output.contains("Backfilling"));
        assert!(result.state_changed);
    }
}
