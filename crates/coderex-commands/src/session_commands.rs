//! Session management commands: /clear, /exit, /session.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

/// Register session management commands.
pub fn register(registry: &mut CommandRegistry) {
    // /clear — clear the current session messages
    registry.register(SlashCommand::local(
        "clear",
        "Clear the current session",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(format!("Session '{}' cleared", ctx.session_id))
                .with_state_change(true)
        }),
    ));

    // /exit — graceful exit
    registry.register(SlashCommand::local(
        "exit",
        "Exit the session",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success("Exiting...").with_suppressed()
        }),
    ));

    // /session — show current session info
    registry.register(SlashCommand::local(
        "session",
        "Show current session info",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let output = format!(
                "Session: {} ({})\nModel: {}\nMessages: {}\nElapsed: {}s\nWorking Dir: {}\nBranch: {}",
                ctx.session_id,
                ctx.session_title,
                ctx.model.0,
                ctx.message_count,
                ctx.elapsed_secs,
                ctx.cwd,
                ctx.git_branch,
            );
            CommandResult::success(output)
        }),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::types::ModelId;

    fn ctx() -> CommandContext {
        CommandContext {
            session_title: "Test".into(),
            message_count: 10,
            elapsed_secs: 120,
            git_branch: "main".into(),
            ..CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
        }
    }

    #[test]
    fn clear_command() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("clear", &ctx(), &[]);
        assert!(result.output.contains("cleared"));
        assert!(result.state_changed);
    }

    #[test]
    fn exit_command() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("exit", &ctx(), &[]);
        assert!(result.output.contains("Exiting"));
        assert!(result.suppress_output);
    }

    #[test]
    fn session_command() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("session", &ctx(), &[]);
        assert!(result.output.contains("s1"));
        assert!(result.output.contains("Test"));
        assert!(result.output.contains("claude-sonnet-4-6"));
        assert!(result.output.contains("10"));
        assert!(result.output.contains("120s"));
        assert!(result.output.contains("main"));
    }
}
