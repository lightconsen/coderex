//! Session extension commands: /resume, /rename, /rewind.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

/// Register session extension commands.
pub fn register(registry: &mut CommandRegistry) {
    // /resume — resume previous session
    registry.register(SlashCommand::local(
        "resume",
        "Resume a previous session",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::success("Resuming most recent session...")
                    .with_state_change(true)
            } else {
                CommandResult::success(format!("Resuming session: {}", args[0]))
                    .with_state_change(true)
            }
        }),
    ));

    // /rename — rename current session
    registry.register(SlashCommand::local(
        "rename",
        "Rename the current session",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::error(format!(
                    "Usage: /rename <new-title>\nCurrent title: {}",
                    ctx.session_title
                ))
            } else {
                let new_title = args.join(" ");
                CommandResult::success(format!(
                    "Session renamed from '{}' to '{}'",
                    ctx.session_title, new_title
                ))
                .with_state_change(true)
            }
        }),
    ));

    // /rewind — rewind to a point in history
    registry.register(SlashCommand::local(
        "rewind",
        "Rewind to a previous point in the session",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::error("Usage: /rewind <message-number>")
            } else {
                match args[0].parse::<usize>() {
                    Ok(n) if n <= ctx.message_count => {
                        CommandResult::success(format!(
                            "Rewound to message {n} of {}",
                            ctx.message_count
                        ))
                        .with_state_change(true)
                    }
                    Ok(n) => {
                        CommandResult::error(format!(
                            "Message {n} not found. Session has {} messages.",
                            ctx.message_count
                        ))
                    }
                    Err(_) => {
                        CommandResult::error("Invalid message number. Usage: /rewind <number>")
                    }
                }
            }
        }),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::types::ModelId;

    fn ctx() -> CommandContext {
        CommandContext {
            message_count: 20,
            ..CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
        }
    }

    #[test]
    fn resume_last_session() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("resume", &ctx(), &[]);
        assert!(result.output.contains("most recent"));
        assert!(result.state_changed);
    }

    #[test]
    fn resume_specific_session() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("resume", &ctx(), &["s_abc".into()]);
        assert!(result.output.contains("s_abc"));
    }

    #[test]
    fn rename_requires_arg() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rename", &ctx(), &[]);
        assert!(result.output.contains("Usage"));
    }

    #[test]
    fn rename_success() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rename", &ctx(), &["New".into(), "Title".into()]);
        assert!(result.output.contains("New Title"));
        assert!(result.state_changed);
    }

    #[test]
    fn rewind_requires_arg() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rewind", &ctx(), &[]);
        assert!(result.output.contains("Usage"));
    }

    #[test]
    fn rewind_valid_number() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rewind", &ctx(), &["5".into()]);
        assert!(result.output.contains("message 5"));
        assert!(result.state_changed);
    }

    #[test]
    fn rewind_out_of_range() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rewind", &ctx(), &["999".into()]);
        assert!(result.output.contains("not found"));
    }

    #[test]
    fn rewind_invalid_number() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("rewind", &ctx(), &["abc".into()]);
        assert!(result.output.contains("Invalid"));
    }
}
