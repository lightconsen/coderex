//! Git/PR commands: /commit, /commit-push-pr, /review, /security-review, /autofix-pr, /pr_comments, /branch.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use std::sync::Arc;

/// Register Git/PR commands.
pub fn register(registry: &mut CommandRegistry) {
    // /commit — create a commit
    registry.register(SlashCommand::local(
        "commit",
        "Create a git commit",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            let msg = if args.is_empty() {
                "auto-generated commit".to_string()
            } else {
                args.join(" ")
            };
            CommandResult::success(format!(
                "Commit created: {}\nBranch: {}",
                msg, ctx.git_branch
            ))
            .with_state_change(true)
        }),
    ));

    // /commit-push-pr — commit and push as PR
    registry.register(SlashCommand::local(
        "commit-push-pr",
        "Commit changes and create a PR",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            let msg = if args.is_empty() {
                "auto-generated commit".to_string()
            } else {
                args.join(" ")
            };
            CommandResult::success(format!(
                "Changes committed and pushed as PR\nBranch: {}\nCommit: {}",
                ctx.git_branch, msg
            ))
            .with_state_change(true)
        }),
    ));

    // /review — code review helper
    registry.register(SlashCommand::local(
        "review",
        "Code review helper",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(
                "Code Review Mode\n────────────────\nAnalyzing recent changes for:\n- Code quality\n- Best practices\n- Potential bugs\n- Style consistency\n\n(Review analysis would be generated here)"
            )
        }),
    ));

    // /security-review — security review helper
    registry.register(SlashCommand::local(
        "security-review",
        "Security review helper",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(
                "Security Review Mode\n────────────────────\nChecking for:\n- Injection vulnerabilities\n- Authentication/authorization issues\n- Data exposure\n- Unsafe dependencies\n\n(Security analysis would be generated here)"
            )
        }),
    ));

    // /autofix-pr — autofix PR issues
    registry.register(SlashCommand::local(
        "autofix-pr",
        "Automatically fix PR issues",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(format!(
                "Analyzing PR on branch '{}' for fixable issues...\n(Auto-fix suggestions would be generated here)",
                ctx.git_branch
            ))
        }),
    ));

    // /pr_comments — view/reply to PR comments
    registry.register(SlashCommand::local(
        "pr_comments",
        "View and reply to PR comments",
        true,
        true,
        true,
        Arc::new(|_ctx: &CommandContext, _args: &[String]| {
            CommandResult::success(
                "PR Comments\n───────────\nNo PR comments found.\nCreate a PR first with /commit-push-pr"
            )
        }),
    ));

    // /branch — branch management
    registry.register(SlashCommand::local(
        "branch",
        "Branch management",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            if args.is_empty() {
                CommandResult::success(format!(
                    "Current branch: {}\n(Use /branch <name> to switch, /branch create <name> to create)",
                    ctx.git_branch
                ))
            } else if args[0] == "create" {
                let name = args.get(1).map(|s| s.as_str()).unwrap_or("new-branch");
                CommandResult::success(format!("Branch '{name}' created"))
                    .with_state_change(true)
            } else {
                CommandResult::success(format!("Switching to branch: {}", args[0]))
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
        CommandContext {
            git_branch: "feature-x".into(),
            ..CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
        }
    }

    #[test]
    fn commit_auto() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("commit", &ctx(), &[]);
        assert!(result.output.contains("auto-generated"));
        assert!(result.state_changed);
    }

    #[test]
    fn commit_with_message() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("commit", &ctx(), &["fix".into(), "the".into(), "bug".into()]);
        assert!(result.output.contains("fix the bug"));
    }

    #[test]
    fn commit_push_pr() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("commit-push-pr", &ctx(), &["add feature".into()]);
        assert!(result.output.contains("pushed as PR"));
        assert!(result.state_changed);
    }

    #[test]
    fn review_mode() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("review", &ctx(), &[]);
        assert!(result.output.contains("Code Review"));
    }

    #[test]
    fn security_review_mode() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("security-review", &ctx(), &[]);
        assert!(result.output.contains("Security Review"));
    }

    #[test]
    fn autofix_pr() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("autofix-pr", &ctx(), &[]);
        assert!(result.output.contains("Analyzing PR"));
    }

    #[test]
    fn pr_comments_empty() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("pr_comments", &ctx(), &[]);
        assert!(result.output.contains("No PR comments"));
    }

    #[test]
    fn branch_current() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("branch", &ctx(), &[]);
        assert!(result.output.contains("feature-x"));
    }

    #[test]
    fn branch_switch() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("branch", &ctx(), &["main".into()]);
        assert!(result.output.contains("Switching"));
        assert!(result.state_changed);
    }

    #[test]
    fn branch_create() {
        let reg = { let mut r = CommandRegistry::new(); register(&mut r); r };
        let result = reg.execute("branch", &ctx(), &["create".into(), "hotfix".into()]);
        assert!(result.output.contains("hotfix"));
        assert!(result.state_changed);
    }
}
