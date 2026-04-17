//! Config commands: /config, /model, /permissions.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use coderex_core::permissions::PermissionMode;
use coderex_core::types::ModelId;
use std::sync::Arc;

/// Register config commands.
pub fn register(registry: &mut CommandRegistry) {
    // /config — show/edit config
    registry.register(SlashCommand::local(
        "config",
        "Show or edit configuration",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::success(format!(
                    "Model: {}\nPermissions: {:?}\nTheme: {}\nVim: {}\nWorking Dir: {}\nMCP Connected: {}",
                    ctx.model.0,
                    ctx.permission_mode,
                    ctx.theme,
                    ctx.vim_mode,
                    ctx.cwd,
                    ctx.mcp_connected,
                ))
            } else {
                CommandResult::success(format!("Setting config key: {}", args[0]))
                    .with_state_change(true)
            }
        }),
    ));

    // /model — switch model
    registry.register(SlashCommand::local(
        "model",
        "Switch AI model",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::error("Usage: /model <opus|sonnet|haiku>")
            } else {
                let model_name = match args[0].to_lowercase().as_str() {
                    "opus" => ModelId::opus().0,
                    "sonnet" => ModelId::sonnet().0,
                    "haiku" => ModelId::haiku().0,
                    other => other.to_string(),
                };
                CommandResult::success(format!("Model switched to: {model_name}"))
                    .with_state_change(true)
            }
        }),
    ));

    // /permissions — view/set permission mode
    registry.register(SlashCommand::local(
        "permissions",
        "View or set permission mode",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::success(format!(
                    "Current permission mode: {:?}\nAvailable: Default, AcceptEdits, BypassPermissions, DontAsk, Plan, Auto",
                    ctx.permission_mode,
                ))
            } else {
                let mode = match args[0].to_lowercase().as_str() {
                    "default" => PermissionMode::Default,
                    "acceptedits" | "accept_edits" => PermissionMode::AcceptEdits,
                    "bypasspermissions" | "bypass" | "bypass_permissions" => PermissionMode::BypassPermissions,
                    "dontask" | "dont_ask" => PermissionMode::DontAsk,
                    "plan" | "planmode" | "plan_mode" => PermissionMode::Plan,
                    "auto" => PermissionMode::Auto,
                    _ => {
                        return CommandResult::error(
                            "Unknown mode. Available: default, acceptedits, bypasspermissions, dontask, plan, auto"
                        );
                    }
                };
                CommandResult::success(format!("Permission mode set to: {mode:?}"))
                    .with_state_change(true)
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
    fn config_shows_current_state() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("config", &ctx(), &[]);
        assert!(result.output.contains("Model"));
        assert!(result.output.contains("Permissions"));
    }

    #[test]
    fn config_key_sets_value() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("config", &ctx(), &["model".into()]);
        assert!(result.output.contains("model"));
        assert!(result.state_changed);
    }

    #[test]
    fn model_switch_requires_arg() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("model", &ctx(), &[]);
        assert!(result.output.contains("Usage"));
    }

    #[test]
    fn model_switch_to_opus() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("model", &ctx(), &["opus".into()]);
        assert!(result.output.contains("claude-opus"));
        assert!(result.state_changed);
    }

    #[test]
    fn model_switch_to_haiku() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("model", &ctx(), &["haiku".into()]);
        assert!(result.output.contains("haiku"));
    }

    #[test]
    fn model_switch_unknown() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("model", &ctx(), &["unknown-model".into()]);
        assert!(result.output.contains("unknown-model"));
    }

    #[test]
    fn permissions_shows_current() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("permissions", &ctx(), &[]);
        assert!(result.output.contains("permission mode"));
    }

    #[test]
    fn permissions_set_bypass() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("permissions", &ctx(), &["bypass".into()]);
        assert!(result.output.contains("BypassPermissions"));
        assert!(result.state_changed);
    }

    #[test]
    fn permissions_set_plan() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("permissions", &ctx(), &["plan".into()]);
        assert!(result.output.contains("Plan"));
    }

    #[test]
    fn permissions_set_unknown_mode() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("permissions", &ctx(), &["foobar".into()]);
        assert!(result.output.contains("Error"));
    }
}
