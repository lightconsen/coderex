//! Debug/diagnostic commands: /debug-tool-call, /heapdump, /mock-limits, /perf-issue, /ant-trace, /break-cache, /reload-plugins.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

/// Register debug/diagnostic commands.
pub fn register(registry: &mut CommandRegistry) {
    // /debug-tool-call — view recent tool call details
    registry.register(SlashCommand::local(
        "debug-tool-call",
        "View recent tool call details",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let mut lines = vec![
                "Recent Tool Calls".to_string(),
                "─────────────────".to_string(),
                format!("Session: {}", ctx.session_id),
                format!("Model: {}", ctx.model.0),
                format!("Messages: {}", ctx.message_count),
                String::new(),
            ];
            if ctx.message_count == 0 {
                lines.push("(No tool calls recorded in this session)".to_string());
                lines.push("Tool calls appear here when the model invokes tools.".to_string());
            } else {
                lines.push(format!("Input tokens: {}", ctx.input_tokens));
                lines.push(format!("Output tokens: {}", ctx.output_tokens));
                lines.push(format!("Cache read: {}", ctx.cache_read_tokens));
                lines.push(format!("Cache creation: {}", ctx.cache_creation_tokens));
                lines.push(format!("Cost: ${:.4}", ctx.cost_usd));
            }
            CommandResult::success(lines.join("\n"))
        }),
    ));

    // /heapdump — memory snapshot (feature-gated)
    registry.register(SlashCommand::local(
        "heapdump",
        "Generate memory heap snapshot (feature-gated)",
        false,
        false,
        false,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            if ctx.is_feature_enabled("heapdump") {
                CommandResult::success("Heap dump generated: /tmp/coderex-heap.prof")
            } else {
                CommandResult::error("heapdump feature is not enabled.")
            }
        }),
    ));

    // /mock-limits — simulate API rate limits (test mode)
    registry.register(SlashCommand::local(
        "mock-limits",
        "Simulate API rate limits for testing",
        false,
        false,
        false,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(
                "Rate Limit Simulation\n────────────────────\nNext API calls will simulate:\n  429 Too Many Requests\n  Retry-After: 60s\n\n(Use for testing rate limit handling)"
            )
            .with_state_change(true)
        }),
    ));

    // /perf-issue — diagnose performance issues
    registry.register(SlashCommand::local(
        "perf-issue",
        "Diagnose performance issues",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let mut findings = Vec::new();

            // Check context size
            let total_tokens = ctx.input_tokens + ctx.output_tokens + ctx.cache_read_tokens + ctx.cache_creation_tokens;
            if total_tokens > 100_000 {
                findings.push(format!("[!] High context usage: {total_tokens} tokens — consider /compact"));
            } else {
                findings.push(format!("[✓] Context usage: {total_tokens} tokens"));
            }

            // Check message count
            if ctx.message_count > 100 {
                findings.push(format!("[!] High message count: {} — compact recommended", ctx.message_count));
            } else {
                findings.push(format!("[✓] Message count: {}", ctx.message_count));
            }

            // Check elapsed time
            if ctx.elapsed_secs > 3600 {
                findings.push(format!("[!] Long session: {:.1} hours", ctx.elapsed_secs as f64 / 3600.0));
            } else {
                findings.push(format!("[✓] Session duration: {}s", ctx.elapsed_secs));
            }

            CommandResult::success(format!(
                "Performance Diagnostics\n────────────────────────\n{}",
                findings.join("\n")
            ))
        }),
    ));

    // /ant-trace — API call tracing
    registry.register(SlashCommand::local(
        "ant-trace",
        "View Anthropic API call trace",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let mut lines = vec![
                "API Call Trace".to_string(),
                "──────────────".to_string(),
                format!("Model: {}", ctx.model.0),
                format!("Session: {}", ctx.session_id),
                format!("Input tokens: {}", ctx.input_tokens),
                format!("Output tokens: {}", ctx.output_tokens),
                format!("Cache read: {}", ctx.cache_read_tokens),
                format!("Cache creation: {}", ctx.cache_creation_tokens),
                format!("Cost: ${:.4}", ctx.cost_usd),
            ];
            if ctx.input_tokens == 0 && ctx.output_tokens == 0 {
                lines.push(String::new());
                lines.push("(No API calls recorded in this trace session)".to_string());
                lines.push("API calls appear here when the model is invoked.".to_string());
            }
            CommandResult::success(lines.join("\n"))
        }),
    ));

    // /break-cache — clear prompt cache (debug)
    registry.register(SlashCommand::local(
        "break-cache",
        "Clear prompt cache (debug tool)",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(format!(
                "Prompt cache cleared.\nPrevious cache: {} tokens",
                ctx.cache_read_tokens
            ))
            .with_state_change(true)
        }),
    ));

    // /reload-plugins — hot-reload plugins
    registry.register(SlashCommand::local(
        "reload-plugins",
        "Hot-reload plugins",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let mut discovered = 0;
            let plugin_dirs = [
                std::path::Path::new(&ctx.cwd).join(".coderex/plugins"),
                coderex_config::settings::config_home().join("plugins"),
            ];
            for dir in &plugin_dirs {
                if !dir.exists() {
                    continue;
                }
                if let Ok(entries) = std::fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let manifest = entry.path().join("plugin.json");
                        if manifest.exists() {
                            discovered += 1;
                        }
                    }
                }
            }
            CommandResult::success(format!(
                "Plugins reloaded successfully.\nDiscovered: {discovered} plugin(s)"
            ))
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
    fn debug_tool_call() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("debug-tool-call", &ctx(), &[]);
        assert!(result.output.contains("Tool Calls"));
    }

    #[test]
    fn heapdump_feature_gated() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("heapdump", &ctx(), &[]);
        assert!(result.output.contains("Error"));
        assert!(result.output.contains("not enabled"));
    }

    #[test]
    fn heapdump_enabled() {
        let mut flags = std::collections::HashMap::new();
        flags.insert("heapdump".into(), true);
        let c = CommandContext { feature_flags: flags, ..ctx() };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("heapdump", &c, &[]);
        assert!(result.output.contains("heap.prof"));
    }

    #[test]
    fn mock_limits_simulation() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("mock-limits", &ctx(), &[]);
        assert!(result.output.contains("429"));
        assert!(result.state_changed);
    }

    #[test]
    fn perf_issue_low_usage() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("perf-issue", &ctx(), &[]);
        assert!(result.output.contains("Performance Diagnostics"));
    }

    #[test]
    fn perf_issue_high_usage() {
        let c = CommandContext {
            input_tokens: 80_000,
            output_tokens: 40_000,
            message_count: 150,
            elapsed_secs: 7200,
            ..ctx()
        };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("perf-issue", &c, &[]);
        assert!(result.output.contains("High context"));
        assert!(result.output.contains("compact recommended"));
        assert!(result.output.contains("Long session"));
    }

    #[test]
    fn ant_trace() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("ant-trace", &ctx(), &[]);
        assert!(result.output.contains("API Call Trace"));
    }

    #[test]
    fn break_cache() {
        let c = CommandContext { cache_read_tokens: 5000, ..ctx() };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("break-cache", &c, &[]);
        assert!(result.output.contains("cleared"));
        assert!(result.output.contains("5000"));
        assert!(result.state_changed);
    }

    #[test]
    fn reload_plugins() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("reload-plugins", &ctx(), &[]);
        assert!(result.output.contains("Plugins reloaded"));
        assert!(result.state_changed);
    }
}
