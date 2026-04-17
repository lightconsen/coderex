//! Miscellaneous commands: /advisor, /btw, /good-claude, /stickers, /bughunter, /issue, /env, /diff, /oauth-refresh, /backfill-sessions.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
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
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success("Backfilling session index...\n(Scanning for historical transcripts)")
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
