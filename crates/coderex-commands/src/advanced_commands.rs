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
            CommandResult::success(
                "Bridge\n──────\nStatus: Not running\nNo remote environments connected.\n\nUse Bridge to run sessions on remote machines."
            )
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
            CommandResult::success("No active Bridge connections to kick.")
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
            CommandResult::success(format!(
                "Project Insights\n────────────────\nWorking Dir: {}\nBranch: {}\nSession Messages: {}\n\n(Project analysis would appear here)",
                ctx.cwd, ctx.git_branch, ctx.message_count
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
                CommandResult::success(
                    "Thinkback\n────────\n(Historical session review would be shown here)\n\nUse /thinkback-play to execute learnings."
                )
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

    // /statusline — statusline custom config
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
}
