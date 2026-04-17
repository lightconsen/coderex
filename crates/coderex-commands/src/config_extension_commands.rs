//! Config extension commands: /theme, /hooks, /mcp, /plugin, /memory, /skills, /tasks, /plan.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

/// Register config extension commands.
pub fn register(registry: &mut CommandRegistry) {
    // /theme — switch/view theme
    registry.register(SlashCommand::local(
        "theme",
        "View or change the UI theme",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::success(format!("Current theme: {}", ctx.theme))
            } else {
                CommandResult::success(format!("Theme changed to: {}", args[0]))
                    .with_state_change(true)
            }
        }),
    ));

    // /hooks — view/manage hooks
    registry.register(SlashCommand::local(
        "hooks",
        "View or manage hooks",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(format!("Active hooks: {}", ctx.active_hooks))
        }),
    ));

    // /mcp — view/manage MCP services
    registry.register(SlashCommand::local(
        "mcp",
        "View or manage MCP services",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.mcp_connected {
                CommandResult::success("MCP: Connected")
            } else {
                CommandResult::success("MCP: Not connected\nUse settings.json to configure MCP servers.")
            }
        }),
    ));

    // /plugin — view/manage plugins
    registry.register(SlashCommand::local(
        "plugin",
        "View or manage plugins",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success("No plugins installed.\nUse /plugin install <name> to install from marketplace.")
        }),
    ));

    // /memory — view/search long-term memory
    registry.register(SlashCommand::local(
        "memory",
        "View or search long-term memory",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::success("Memory: 0 entries\nUse /memory search <query> to search.")
            } else {
                CommandResult::success(format!("Searching memory for: {}", args.join(" ")))
            }
        }),
    ));

    // /skills — view/load skills
    registry.register(SlashCommand::local(
        "skills",
        "View or load skills",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success("Skills:\n  No skills loaded.\nPlace skill scripts in ~/.claude/skills/")
        }),
    ));

    // /tasks — view task list (command version)
    registry.register(SlashCommand::local(
        "tasks",
        "View the task list",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success("Tasks:\n  No tasks registered.\nUse TaskCreate to create tasks.")
        }),
    ));

    // /plan — view/edit current plan
    registry.register(SlashCommand::local(
        "plan",
        "View or edit the current plan",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.has_plan {
                CommandResult::success("Plan loaded.\n\n(Plan content would be shown here)")
            } else {
                CommandResult::success("No active plan.\nUse /plan create to generate one.")
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
    fn theme_shows_current() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("theme", &ctx(), &[]);
        assert!(result.output.contains("Current theme"));
    }

    #[test]
    fn theme_changes() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("theme", &ctx(), &["dark".into()]);
        assert!(result.output.contains("dark"));
        assert!(result.state_changed);
    }

    #[test]
    fn hooks_shows_count() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("hooks", &ctx(), &[]);
        assert!(result.output.contains("Active hooks"));
    }

    #[test]
    fn mcp_status_not_connected() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("mcp", &ctx(), &[]);
        assert!(result.output.contains("Not connected"));
    }

    #[test]
    fn mcp_status_connected() {
        let c = CommandContext { mcp_connected: true, ..ctx() };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("mcp", &c, &[]);
        assert!(result.output.contains("Connected"));
    }

    #[test]
    fn plugin_shows_none() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("plugin", &ctx(), &[]);
        assert!(result.output.contains("No plugins"));
    }

    #[test]
    fn memory_search() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("memory", &ctx(), &["user".into(), "preferences".into()]);
        assert!(result.output.contains("user preferences"));
    }

    #[test]
    fn skills_shows_none() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("skills", &ctx(), &[]);
        assert!(result.output.contains("No skills"));
    }

    #[test]
    fn tasks_shows_none() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("tasks", &ctx(), &[]);
        assert!(result.output.contains("No tasks"));
    }

    #[test]
    fn plan_no_plan() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("plan", &ctx(), &[]);
        assert!(result.output.contains("No active plan"));
    }

    #[test]
    fn plan_loaded() {
        let c = CommandContext { has_plan: true, ..ctx() };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("plan", &c, &[]);
        assert!(result.output.contains("Plan loaded"));
    }
}
