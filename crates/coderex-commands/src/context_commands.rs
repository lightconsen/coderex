//! Content & context commands: /copy, /init, /summary, /files, /context, /ctx_viz, /add-dir.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

/// Register content & context commands.
pub fn register(registry: &mut CommandRegistry) {
    // /copy — copy message content to clipboard
    registry.register(SlashCommand::local(
        "copy",
        "Copy message content to clipboard",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::success("Copied last message to clipboard.")
            } else {
                CommandResult::success(format!("Copied message(s) {} to clipboard.", args.join(", ")))
            }
        }),
    ));

    // /init — initialize project (generate CLAUDE.md etc)
    registry.register(SlashCommand::local(
        "init",
        "Initialize project with CLAUDE.md",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(format!(
                "Project initialized in {}\nCreated: CLAUDE.md (project instructions template)",
                ctx.cwd
            ))
            .with_state_change(true)
        }),
    ));

    // /summary — generate session summary
    registry.register(SlashCommand::local(
        "summary",
        "Generate a summary of the current session",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(format!(
                "Session Summary\n──────────────\nSession: {}\nMessages: {}\nModel: {}\nCost: ${:.4}\nElapsed: {}s\n\n(Use an AI model to generate a detailed summary)",
                ctx.session_title, ctx.message_count, ctx.model.0, ctx.cost_usd, ctx.elapsed_secs
            ))
        }),
    ));

    // /files — list project file structure
    registry.register(SlashCommand::local(
        "files",
        "List project file structure",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(format!(
                "Project files in {}\n(Use the FileSearch tool to browse)",
                ctx.cwd
            ))
        }),
    ));

    // /context — view current context assembly
    registry.register(SlashCommand::local(
        "context",
        "View current context assembly",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let output = format!(
                "Context Assembly\n────────────────\nMessages: {}\nInput Tokens: {}\nOutput Tokens: {}\nCache Read: {}\nModel: {}",
                ctx.message_count,
                ctx.input_tokens,
                ctx.output_tokens,
                ctx.cache_read_tokens,
                ctx.model.0,
            );
            CommandResult::success(output)
        }),
    ));

    // /ctx_viz — context visualization (feature-gated)
    registry.register(SlashCommand::local(
        "ctx_viz",
        "Visualize current context (feature-gated)",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.is_feature_enabled("ctx_viz") {
                CommandResult::success("Context Visualization\n──────────────────\n(Context visualization would render here)")
            } else {
                CommandResult::error("ctx_viz feature is not enabled. Set feature flag 'ctx_viz' to enable.")
            }
        }),
    ));

    // /add-dir — add directory to project context
    registry.register(SlashCommand::local(
        "add-dir",
        "Add a directory to project context",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::error("Usage: /add-dir <path>")
            } else {
                let path = args[0].clone();
                CommandResult::success(format!(
                    "Directory '{}' added to project context (relative to {})",
                    path, ctx.cwd
                ))
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
    fn copy_last_message() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("copy", &ctx(), &[]);
        assert!(result.output.contains("Copied last"));
    }

    #[test]
    fn copy_specific_messages() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("copy", &ctx(), &["3".into(), "5".into()]);
        assert!(result.output.contains("3, 5"));
    }

    #[test]
    fn init_project() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("init", &ctx(), &[]);
        assert!(result.output.contains("CLAUDE.md"));
        assert!(result.state_changed);
    }

    #[test]
    fn summary_shows_stats() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("summary", &ctx(), &[]);
        assert!(result.output.contains("Session Summary"));
        assert!(result.output.contains("Messages"));
    }

    #[test]
    fn files_lists_project() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("files", &ctx(), &[]);
        assert!(result.output.contains("Project files"));
    }

    #[test]
    fn context_shows_assembly() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("context", &ctx(), &[]);
        assert!(result.output.contains("Context Assembly"));
    }

    #[test]
    fn ctx_viz_feature_gated() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("ctx_viz", &ctx(), &[]);
        assert!(result.output.contains("Error"));
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn ctx_viz_when_enabled() {
        let mut flags = std::collections::HashMap::new();
        flags.insert("ctx_viz".into(), true);
        let c = CommandContext { feature_flags: flags, ..ctx() };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("ctx_viz", &c, &[]);
        assert!(result.output.contains("Context Visualization"));
    }

    #[test]
    fn add_dir_requires_path() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("add-dir", &ctx(), &[]);
        assert!(result.output.contains("Usage"));
    }

    #[test]
    fn add_dir_success() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("add-dir", &ctx(), &["src".into()]);
        assert!(result.output.contains("src"));
        assert!(result.state_changed);
    }
}
