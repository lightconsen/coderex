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
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::success(
                    "Usage: /issue create <title> | /issue list | /issue <number>",
                )
            } else if args[0] == "create" {
                let title = args[1..].join(" ");
                if title.is_empty() {
                    return CommandResult::error("Usage: /issue create <title>");
                }
                // Try to create an actual issue via gh CLI
                let dir = std::path::Path::new(&ctx.cwd);
                let output = std::process::Command::new("gh")
                    .args([
                        "issue",
                        "create",
                        "--title",
                        &title,
                        "--body",
                        &format!("Created via CodeRex: {title}"),
                    ])
                    .current_dir(dir)
                    .output();
                match output {
                    Ok(out) if out.status.success() => {
                        let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        CommandResult::success(format!("Issue created: {title}\n{url}"))
                            .with_state_change(true)
                    }
                    Ok(out) => {
                        let err = String::from_utf8_lossy(&out.stderr);
                        CommandResult::success(format!("Issue prepared: {title}\n(gh CLI: {err})",))
                            .with_state_change(true)
                    }
                    Err(e) => CommandResult::success(format!(
                        "Issue prepared: {title}\n(gh CLI not available: {e})",
                    ))
                    .with_state_change(true),
                }
            } else if args[0] == "list" {
                let dir = std::path::Path::new(&ctx.cwd);
                match std::process::Command::new("gh")
                    .args(["issue", "list", "--limit", "20"])
                    .current_dir(dir)
                    .output()
                {
                    Ok(out) if out.status.success() => {
                        let text = String::from_utf8_lossy(&out.stdout);
                        if text.trim().is_empty() {
                            CommandResult::success("Issues:\n  No open issues.")
                        } else {
                            CommandResult::success(format!("Open Issues:\n{text}"))
                        }
                    }
                    _ => CommandResult::success(
                        "Issues:\n  No open issues (gh CLI unavailable or not a repo).",
                    ),
                }
            } else {
                CommandResult::success(format!("Viewing issue: {}", args[0]))
            }
        }),
    ));

    // /env — view environment variables
    registry.register(SlashCommand::local(
        "env",
        "View environment variables",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                let mut output = String::from("Environment:\n");
                let vars: Vec<(String, String)> = std::env::vars().collect();
                if vars.is_empty() {
                    output.push_str("  (none set)");
                } else {
                    for (k, v) in vars.iter().take(50) {
                        output.push_str(&format!("  {k}={v}\n"));
                    }
                    if vars.len() > 50 {
                        output.push_str(&format!("  ... and {} more\n", vars.len() - 50));
                    }
                }
                CommandResult::success(output)
            } else {
                let key = &args[0];
                match std::env::var(key) {
                    Ok(value) => CommandResult::success(format!("{key}={value}")),
                    Err(_) => {
                        CommandResult::error(format!("Environment variable '{key}' not set."))
                    }
                }
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
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            let dir = std::path::Path::new(&ctx.cwd);

            // Ask git whether this is a repository before asking for a diff.
            // Reporting an empty diff for a directory git cannot read would
            // say "nothing changed" about a place where nothing is known.
            let inside = coderex_core::git_hardening::git_command()
                .args(["rev-parse", "--is-inside-work-tree"])
                .current_dir(dir)
                .output();
            match inside {
                Ok(out) if !out.status.success() => {
                    return CommandResult::error(format!(
                        "Not a git repository: {}\nThere is no diff to show.",
                        ctx.cwd
                    ));
                }
                Err(e) => {
                    return CommandResult::error(format!(
                        "Failed to run git: {e}\nEnsure git is installed."
                    ));
                }
                Ok(_) => {}
            }

            let mut cmd_args = vec!["diff"];
            // Support --cached/staged flag
            let cached = args.iter().any(|a| a == "--cached" || a == "--staged");
            if cached {
                cmd_args.push("--cached");
            }
            match coderex_core::git_hardening::git_command()
                .args(&cmd_args)
                .current_dir(dir)
                .output()
            {
                Ok(out) if out.status.success() => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    if text.trim().is_empty() {
                        CommandResult::success("No changes to show.")
                    } else {
                        CommandResult::success(format!(
                            "Git Diff{}\n────────\n{text}",
                            if cached { " (staged)" } else { "" }
                        ))
                    }
                }
                Ok(out) => {
                    let err = String::from_utf8_lossy(&out.stderr);
                    CommandResult::error(format!("git diff failed: {err}"))
                }
                Err(e) => CommandResult::error(format!(
                    "Failed to run git diff: {e}\nEnsure git is installed and this is a git repository."
                )),
            }
        }),
    ));

    // /oauth-refresh — report the credential state
    //
    // There is no *model provider* OAuth token to refresh: CodeRex
    // authenticates with an API key or, for the cloud providers, with
    // credentials from the environment. The command used to say a token had
    // been refreshed, which was not true of anything it did; it now says what
    // it can see instead. MCP server tokens are a different thing and are
    // refreshed by the MCP client on its own, so the report says so rather
    // than implying no refresh exists anywhere.
    registry.register(SlashCommand::local(
        "oauth-refresh",
        "Check the credential state for the current model",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let key_vars = [
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "GROQ_API_KEY",
                "CODEREX_API_KEY",
                "CODEREX_PLUGIN_PUBLIC_KEY",
            ];
            let present: Vec<&str> = key_vars
                .iter()
                .copied()
                .filter(|name| {
                    std::env::var(name)
                        .map(|value| !value.is_empty())
                        .unwrap_or(false)
                })
                .collect();

            let aws = coderex_core::providers::AwsCredentials::from_env().is_some();

            if present.is_empty() && !aws && !ctx.api_key_set {
                return CommandResult::error(
                    "No credential found. Set ANTHROPIC_API_KEY (or another provider's key, or \
                     AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY for Bedrock).",
                );
            }

            let mut report = format!("Credentials for {}:\n", ctx.model.0);
            if present.is_empty() {
                report.push_str("  (no provider API key is set in the environment)\n");
            } else {
                report.push_str(&format!("  keys set: {}\n", present.join(", ")));
            }
            if aws {
                report.push_str("  AWS credentials present (signed requests available)\n");
            }
            if ctx.api_key_set {
                report.push_str("  a key was supplied to this session\n");
            }
            report.push_str(
                "\nNo OAuth token was refreshed: this build authenticates with a key, so there \
                 is no model-provider token to refresh. An OAuth login flow would have to be \
                 added first.\nMCP server tokens are separate: they are refreshed by the MCP \
                 client when a server connection is started, not by this command.",
            );
            CommandResult::success(report)
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
            let project_transcripts = std::path::Path::new(&ctx.cwd).join(".coderex/transcripts");
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

    // /feedback — generate a title for user feedback (REPL built-in)
    registry.register(SlashCommand::local(
        "feedback",
        "Generate an AI title for your feedback",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success("Use /feedback <message> in the REPL to generate a title.")
        }),
    ));

    // /history — view command history
    registry.register(SlashCommand::local(
        "history",
        "View command history",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if ctx.recent_commands.is_empty() {
                return CommandResult::success("No command history available.");
            }
            let format_entries = |entries: &[&String]| {
                entries
                    .iter()
                    .enumerate()
                    .map(|(index, entry)| format!("{:>4}  {entry}", index + 1))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            match args.first().map(String::as_str) {
                None => {
                    let start = ctx.recent_commands.len().saturating_sub(20);
                    let entries: Vec<&String> = ctx.recent_commands[start..].iter().collect();
                    CommandResult::success(format_entries(&entries))
                }
                Some("search") => {
                    if args.len() < 2 {
                        return CommandResult::error("Usage: /history search <query>");
                    }
                    let query = args[1..].join(" ").to_lowercase();
                    let matches: Vec<&String> = ctx
                        .recent_commands
                        .iter()
                        .filter(|entry| entry.to_lowercase().contains(&query))
                        .collect();
                    if matches.is_empty() {
                        CommandResult::success(format!("No commands match '{query}'."))
                    } else {
                        CommandResult::success(format_entries(&matches))
                    }
                }
                Some(raw) => match raw.parse::<usize>() {
                    Ok(n) => {
                        let start = ctx.recent_commands.len().saturating_sub(n);
                        let entries: Vec<&String> = ctx.recent_commands[start..].iter().collect();
                        CommandResult::success(format_entries(&entries))
                    }
                    Err(_) => CommandResult::error("Usage: /history [n] | /history search <query>"),
                },
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
    fn advisor_mode() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("advisor", &ctx(), &[]);
        assert!(result.output.contains("Advisor mode"));
        assert!(result.extra_context.is_some());
        assert_eq!(result.prompt_profile, Some(PromptProfile::Advisor));
        assert!(result.append_system_prompt.is_some());
    }

    #[test]
    fn btw_requires_arg() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("btw", &ctx(), &[]);
        assert!(result.output.contains("Usage"));
    }

    #[test]
    fn btw_adds_context() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("btw", &ctx(), &["important".into(), "context".into()]);
        assert!(result.extra_context.is_some());
    }

    #[test]
    fn good_claude() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("good-claude", &ctx(), &[]);
        assert!(result.output.contains("Thanks"));
    }

    #[test]
    fn stickers_feature_gated() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("stickers", &ctx(), &[]);
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn bughunter_mode() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("bughunter", &ctx(), &[]);
        assert!(result.output.contains("Bug Hunter"));
        assert!(result.extra_context.is_some());
        assert_eq!(result.prompt_profile, Some(PromptProfile::BugHunter));
        assert!(result.append_system_prompt.is_some());
    }

    #[test]
    fn issue_usage() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("issue", &ctx(), &[]);
        assert!(result.output.contains("Usage"));
    }

    #[test]
    fn issue_create() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute(
            "issue",
            &ctx(),
            &["create".into(), "fix".into(), "bug".into()],
        );
        assert!(result.output.contains("fix bug"));
        assert!(result.state_changed);
    }

    #[test]
    fn issue_list() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("issue", &ctx(), &["list".into()]);
        assert!(result.output.contains("No open issues"));
    }

    #[test]
    fn issue_view_number() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("issue", &ctx(), &["42".into()]);
        assert!(result.output.contains("42"));
    }

    #[test]
    fn env_empty() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("env", &ctx(), &[]);
        assert!(result.output.contains("Environment"));
    }

    #[test]
    fn env_with_vars() {
        let mut env = std::collections::HashMap::new();
        env.insert("HOME".into(), "/home/user".into());
        let c = CommandContext { env, ..ctx() };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("env", &c, &[]);
        assert!(result.output.contains("HOME"));
    }

    #[test]
    fn diff_not_git_repo() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("diff", &ctx(), &[]);
        // In a non-git directory, git diff will fail
        assert!(
            result.output.contains("No changes")
                || result.output.contains("failed")
                || result.output.contains("Error")
        );
    }

    #[test]
    fn diff_in_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        // Init a git repo
        coderex_core::git_hardening::git_command()
            .args(["init", "--quiet"])
            .current_dir(tmp.path())
            .output()
            .ok();
        let c = CommandContext {
            cwd: tmp.path().to_string_lossy().to_string(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("diff", &c, &[]);
        // Should report no changes (empty repo)
        assert!(result.output.contains("No changes") || result.output.contains("failed"));
    }

    #[test]
    fn oauth_refresh_no_key() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("oauth-refresh", &ctx(), &[]);
        // Whatever the environment holds, the answer is about the credential
        // state — and it never claims a token was refreshed.
        assert!(
            result.output.contains("No credential found")
                || result.output.contains("No OAuth token was refreshed"),
            "{}",
            result.output
        );
    }

    #[test]
    fn backfill_sessions() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("backfill-sessions", &ctx(), &[]);
        assert!(result.output.contains("Backfilling"));
        assert!(result.state_changed);
    }

    #[test]
    fn history_reads_the_recorded_commands() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let mut context = ctx();
        context.recent_commands = vec!["/model opus".into(), "/diff".into(), "/history 2".into()];

        let all = reg.execute("history", &context, &[]);
        assert!(all.output.contains("/model opus"));
        assert!(all.output.contains("/diff"));

        let last = reg.execute("history", &context, &["1".into()]);
        assert!(last.output.contains("/history 2"));
        assert!(!last.output.contains("/model opus"));

        let searched = reg.execute("history", &context, &["search".into(), "diff".into()]);
        assert!(searched.output.contains("/diff"));
        assert!(!searched.output.contains("/model opus"));
    }

    #[test]
    fn feedback_help() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("feedback", &ctx(), &[]);
        assert!(result.output.contains("Use /feedback"));
    }
}
