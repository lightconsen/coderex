//! Content & integration commands: /compact, /help, /version.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

/// Register content & integration commands.
pub fn register(registry: &mut CommandRegistry) {
    // /compact — manually trigger compact
    registry.register(SlashCommand::local(
        "compact",
        "Trigger context compact",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(format!(
                "Compacting session {} ({} messages)...",
                ctx.session_id, ctx.message_count
            ))
            .with_state_change(true)
        }),
    ));

    // /help — show help
    registry.register(SlashCommand::local(
        "help",
        "Show available commands",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                let mut output = String::from("Available Commands:\n");
                output.push_str(&format!("  Working Dir: {}\n\n", ctx.cwd));
                output.push_str("Commands (use /<command>):\n");
                output.push_str("  Session:  /clear, /exit, /session, /resume, /rename, /rewind\n");
                output.push_str("  Config:   /config, /model, /permissions, /theme, /hooks, /mcp, /plugin, /memory, /skills, /tasks, /plan\n");
                output.push_str("  Stats:    /stats, /cost, /usage, /tokens, /status, /doctor\n");
                output.push_str("  Content:  /compact, /help, /version, /copy, /init, /summary, /files, /open, /context, /add-dir\n");
                output.push_str("  Git/PR:   /commit, /commit-push-pr, /review, /security-review, /branch, /diff\n");
                output.push_str("  Advanced: /bridge, /insights, /statusline\n");
                output.push_str("  Misc:     /advisor, /btw, /good-claude, /bughunter, /issue, /env\n");
                CommandResult::success(output)
            } else {
                let cmd_name = &args[0];
                CommandResult::success(format!("Help for /{cmd_name}: (use /help for full list)"))
            }
        }),
    ));

    // /version — show version
    registry.register(SlashCommand::local(
        "version",
        "Show version info",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let version = if ctx.version.is_empty() {
                "0.1.0-dev"
            } else {
                &ctx.version
            };
            CommandResult::success(format!("CodeRex {version}\nRust workspace\n"))
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
    fn compact_triggers() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("compact", &ctx(), &[]);
        assert!(result.output.contains("Compacting"));
        assert!(result.state_changed);
    }

    #[test]
    fn help_shows_commands() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("help", &ctx(), &[]);
        assert!(result.output.contains("clear"));
        assert!(result.output.contains("config"));
        assert!(result.output.contains("stats"));
    }

    #[test]
    fn help_for_specific_command() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("help", &ctx(), &["clear".into()]);
        assert!(result.output.contains("/clear"));
    }

    #[test]
    fn version_shows_info() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("version", &ctx(), &[]);
        assert!(result.output.contains("CodeRex"));
    }

    #[test]
    fn version_with_custom_version() {
        let c = CommandContext {
            version: "1.2.3".into(),
            ..ctx()
        };
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("version", &c, &[]);
        assert!(result.output.contains("1.2.3"));
    }
}
