//! Diagnostic commands: /status, /doctor.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

/// Register diagnostic commands.
pub fn register(registry: &mut CommandRegistry) {
    // /status — show system status
    registry.register(SlashCommand::local(
        "status",
        "Show system status",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let mut lines = Vec::<String>::new();
            lines.push("System Status".to_string());
            lines.push("─────────────".to_string());
            lines.push(format!("Session: {} ({})", ctx.session_id, ctx.session_title));
            lines.push(format!("Model: {}", ctx.model.0));
            lines.push(format!("Messages: {}", ctx.message_count));
            lines.push(format!("Cost: ${:.4}", ctx.cost_usd));
            lines.push(format!("Git Branch: {}", ctx.git_branch));
            lines.push(format!("MCP: {}", if ctx.mcp_connected { "Connected" } else { "Disconnected" }));
            lines.push(format!("Hooks: {} active", ctx.active_hooks));
            lines.push(format!("Elapsed: {}s", ctx.elapsed_secs));
            lines.push(format!("API Key: {}", if ctx.api_key_set { "Set" } else { "Not set" }));
            CommandResult::success(lines.join("\n"))
        }),
    ));

    // /doctor — diagnose environment health
    registry.register(SlashCommand::local(
        "doctor",
        "Diagnose environment health",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let mut checks = Vec::<String>::new();
            let mut issues = 0;

            // API Key
            if ctx.api_key_set {
                checks.push("[✓] API key is set".to_string());
            } else {
                checks.push("[✗] API key is NOT set".to_string());
                issues += 1;
            }

            // Working directory
            if ctx.cwd.is_empty() {
                checks.push("[✗] Working directory not set".to_string());
                issues += 1;
            } else {
                checks.push(format!("[✓] Working directory: {}", ctx.cwd));
            }

            // Model
            checks.push(format!("[✓] Model: {}", ctx.model.0));

            // MCP
            if ctx.mcp_connected {
                checks.push("[✓] MCP connected".to_string());
            } else {
                checks.push("[ℹ] MCP not connected (optional)".to_string());
            }

            // Git
            if ctx.git_branch.is_empty() {
                checks.push("[ℹ] No git repository detected (optional)".to_string());
            } else {
                checks.push(format!("[✓] Git branch: {}", ctx.git_branch));
            }

            // Permissions
            checks.push(format!("[✓] Permission mode: {:?}", ctx.permission_mode));

            // Feature flags summary
            let enabled = ctx.feature_flags.values().filter(|&&v| v).count();
            let disabled = ctx.feature_flags.len() - enabled;
            checks.push(format!("[✓] Feature flags: {enabled} enabled, {disabled} disabled"));

            let summary = if issues == 0 {
                "All critical checks passed."
            } else {
                &format!("{issues} issue(s) found.")
            };

            let mut output = checks.join("\n");
            output.push_str("\n\n");
            output.push_str(summary);
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
            api_key_set: true,
            mcp_connected: true,
            git_branch: "main".into(),
            ..CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
        }
    }

    #[test]
    fn status_shows_info() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("status", &ctx(), &[]);
        assert!(result.output.contains("Session"));
        assert!(result.output.contains("Model"));
        assert!(result.output.contains("MCP"));
        assert!(result.output.contains("API Key"));
    }

    #[test]
    fn doctor_all_good() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("doctor", &ctx(), &[]);
        assert!(result.output.contains("All critical checks"));
        assert!(!result.output.contains("issue(s)"));
    }

    #[test]
    fn doctor_missing_api_key() {
        let c = CommandContext { api_key_set: false, ..ctx() };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("doctor", &c, &[]);
        assert!(result.output.contains("API key is NOT set"));
    }

    #[test]
    fn doctor_no_git() {
        let c = CommandContext { git_branch: String::new(), ..ctx() };
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("doctor", &c, &[]);
        assert!(result.output.contains("No git repository"));
    }
}
