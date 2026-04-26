//! Stats commands: /stats, /cost, /usage, /tokens.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

/// Register stats commands.
pub fn register(registry: &mut CommandRegistry) {
    // /stats — show session stats
    registry.register(SlashCommand::local(
        "stats",
        "Show session statistics",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let output = format!(
                "Session: {} ({})\nMessages: {}\nElapsed: {}\n\nTokens:\n  Input:  {:>8}\n  Output: {:>8}\n  Cache Read: {:>8}\n  Cache Creation: {:>8}\n\nCost: ${:.4}\nModel: {}\nPermissions: {:?}",
                ctx.session_id,
                ctx.session_title,
                ctx.message_count,
                format_duration(ctx.elapsed_secs),
                ctx.input_tokens,
                ctx.output_tokens,
                ctx.cache_read_tokens,
                ctx.cache_creation_tokens,
                ctx.cost_usd,
                ctx.model.0,
                ctx.permission_mode,
            );
            CommandResult::success(output)
        }),
    ));

    // /cost — show cost stats
    registry.register(SlashCommand::local(
        "cost",
        "Show cost statistics",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let output = if ctx.cost_summary.is_empty() {
                format!(
                    "Session Cost: ${:.6}\nModel: {}\nInput Tokens: {}\nOutput Tokens: {}\nCache Read: {}\nCache Creation: {}",
                    ctx.cost_usd,
                    ctx.model.0,
                    ctx.input_tokens,
                    ctx.output_tokens,
                    ctx.cache_read_tokens,
                    ctx.cache_creation_tokens,
                )
            } else {
                ctx.cost_summary.clone()
            };
            CommandResult::success(output)
        }),
    ));

    // /usage — show token usage
    registry.register(SlashCommand::local(
        "usage",
        "Show token usage",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let total = ctx.input_tokens
                + ctx.output_tokens
                + ctx.cache_read_tokens
                + ctx.cache_creation_tokens;
            let output = format!(
                "Token Usage\n───────────\nInput:          {:>8} tokens\nOutput:         {:>8} tokens\nCache Read:     {:>8} tokens\nCache Creation: {:>8} tokens\n───────────\nTotal:          {:>8} tokens",
                ctx.input_tokens,
                ctx.output_tokens,
                ctx.cache_read_tokens,
                ctx.cache_creation_tokens,
                total,
            );
            CommandResult::success(output)
        }),
    ));

    // /tokens — alias for /usage
    registry.register(SlashCommand::local(
        "tokens",
        "Show token usage",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let total = ctx.input_tokens
                + ctx.output_tokens
                + ctx.cache_read_tokens
                + ctx.cache_creation_tokens;
            let output = format!(
                "Token Usage\n───────────\nInput:          {:>8} tokens\nOutput:         {:>8} tokens\nCache Read:     {:>8} tokens\nCache Creation: {:>8} tokens\n───────────\nTotal:          {:>8} tokens",
                ctx.input_tokens,
                ctx.output_tokens,
                ctx.cache_read_tokens,
                ctx.cache_creation_tokens,
                total,
            );
            CommandResult::success(output)
        }),
    ));
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::types::ModelId;

    fn ctx() -> CommandContext {
        CommandContext {
            input_tokens: 5000,
            output_tokens: 3000,
            cache_read_tokens: 1000,
            cache_creation_tokens: 500,
            cost_usd: 0.042,
            message_count: 15,
            elapsed_secs: 3661,
            ..CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
        }
    }

    #[test]
    fn stats_shows_all_data() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("stats", &ctx(), &[]);
        assert!(result.output.contains("5000"));
        assert!(result.output.contains("3000"));
        assert!(result.output.contains("$0.04"));
        assert!(result.output.contains("1h 1m 1s"));
        assert!(result.output.contains("15"));
    }

    #[test]
    fn cost_shows_monetary_value() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("cost", &ctx(), &[]);
        assert!(result.output.contains("$0.04"));
        assert!(result.output.contains("Input Tokens"));
    }

    #[test]
    fn usage_shows_token_breakdown() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("usage", &ctx(), &[]);
        assert!(result.output.contains("5000"));
        assert!(result.output.contains("3000"));
        assert!(result.output.contains("9500"));
    }

    #[test]
    fn tokens_alias_matches_usage() {
        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let usage = reg.execute("usage", &ctx(), &[]);
        let tokens = reg.execute("tokens", &ctx(), &[]);
        assert_eq!(usage.output, tokens.output);
    }
}
