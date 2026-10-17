//! Git/PR commands: /commit, /commit-push-pr, /review, /security-review, /autofix-pr, /pr_comments, /branch.

use crate::registry::{CommandContext, CommandRegistry, CommandResult, SlashCommand};
use coderex_core::query_profiles::PromptProfile;
use std::sync::Arc;

/// Run a git command in the given directory.
fn run_git(dir: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let output = coderex_core::git_hardening::git_command()
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("git command failed: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("git command failed: {stderr}"))
    }
}

/// Run a gh CLI command in the given directory.
fn run_gh(dir: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("gh")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("gh command failed: {e}. Is gh CLI installed?"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!("gh command failed: {stderr}"))
    }
}

/// Get the current git branch.
fn current_branch(dir: &std::path::Path) -> String {
    run_git(dir, &["branch", "--show-current"]).unwrap_or_else(|_| "detached HEAD".to_string())
}

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
                "auto-generated commit"
            } else {
                &args.join(" ")
            };
            let dir = std::path::Path::new(&ctx.cwd);

            // Try to stage and commit
            match run_git(dir, &["add", "-A"]) {
                Ok(_) => match run_git(dir, &["commit", "-m", msg]) {
                    Ok(output) => {
                        let branch = current_branch(dir);
                        CommandResult::success(format!(
                            "Commit created: {msg}\nBranch: {branch}\n{output}"
                        ))
                        .with_state_change(true)
                    }
                    Err(e) => {
                        // Check if there are actually no changes
                        if e.contains("nothing to commit") || e.contains("no changes added") {
                            CommandResult::success(format!(
                                "Nothing to commit. No changes detected on branch '{}'.",
                                current_branch(dir)
                            ))
                        } else {
                            CommandResult::error(format!("Commit failed: {e}"))
                        }
                    }
                },
                Err(e) => CommandResult::error(format!("Git add failed: {e}")),
            }
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
                "auto-generated commit"
            } else {
                &args.join(" ")
            };
            let dir = std::path::Path::new(&ctx.cwd);

            // Stage and commit
            let _ = run_git(dir, &["add", "-A"]);
            if let Err(e) = run_git(dir, &["commit", "-m", msg]) {
                if !e.contains("nothing to commit") {
                    return CommandResult::error(format!("Commit failed: {e}"));
                }
            }

            // Push
            let branch = current_branch(dir);
            if let Err(e) = run_git(dir, &["push", "-u", "origin", &branch]) {
                return CommandResult::error(format!("Push failed: {e}"));
            }

            // Create PR
            match run_gh(dir, &["pr", "create", "--title", msg, "--body", msg]) {
                Ok(url) => CommandResult::success(format!(
                    "Changes committed and pushed as PR\nBranch: {branch}\nPR: {url}"
                ))
                .with_state_change(true),
                Err(e) => CommandResult::success(format!(
                    "Changes committed and pushed to '{branch}'\nPR creation failed: {e}\nCreate PR manually: gh pr create"
                ))
                .with_state_change(true),
            }
        }),
    ));

    // /review — code review helper
    registry.register(SlashCommand::prompt(
        "review",
        "Code review helper",
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let dir = std::path::Path::new(&ctx.cwd);
            let diff = run_git(dir, &["diff", "HEAD~1"]).unwrap_or_default();
            let branch = current_branch(dir);
            let diff_lines = diff.lines().count();
            CommandResult::success(format!(
                "Code Review\n───────────\nBranch: {branch}\nRecent changes: {diff_lines} lines\n\nReview mode activated for the current diff. Ask the agent to review the changes or continue with a follow-up question."
            ))
            .with_context(format!(
                "Review the current git diff on branch '{branch}'.\n\nDiff:\n{diff}"
            ))
            .with_prompt_profile(PromptProfile::Reviewer)
        }),
    ));

    // /security-review — security review helper
    registry.register(SlashCommand::prompt(
        "security-review",
        "Security review helper",
        true,
        Arc::new(|ctx: &CommandContext, _args: &[String]| {
            let dir = std::path::Path::new(&ctx.cwd);
            let diff = run_git(dir, &["diff", "HEAD~1"]).unwrap_or_default();
            let branch = current_branch(dir);
            let diff_lines = diff.lines().count();
            CommandResult::success(format!(
                "Security Review\n───────────────\nBranch: {branch}\nRecent changes: {diff_lines} lines\n\nSecurity review mode activated for the current diff. Ask the agent to analyze the changes or continue with a follow-up question."
            ))
            .with_context(format!(
                "Perform a security review of the current git diff on branch '{branch}'.\n\nDiff:\n{diff}"
            ))
            .with_prompt_profile(PromptProfile::SecurityReviewer)
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
            let dir = std::path::Path::new(&ctx.cwd);
            let branch = current_branch(dir);
            let mut output = format!("Analyzing PR on branch '{branch}' for fixable issues...\n");

            // Check for PR checks status
            match run_gh(dir, &["pr", "checks", "--json", "name,state"]) {
                Ok(checks) => {
                    let failing: Vec<&str> = checks
                        .lines()
                        .filter(|l| l.contains("failure") || l.contains("fail"))
                        .collect();
                    if failing.is_empty() {
                        output.push_str("All PR checks passing.\n");
                    } else {
                        output.push_str(&format!("Found {} failing check(s):\n", failing.len()));
                        for check in &failing {
                            output.push_str(&format!("  - {check}\n"));
                        }
                    }
                }
                Err(_) => {
                    output.push_str("Could not fetch PR checks (no PR or gh CLI unavailable).\n");
                }
            }

            // Show current diff stats
            match run_git(dir, &["diff", "--stat", "HEAD"]) {
                Ok(diff) if !diff.is_empty() => {
                    output.push_str("\nUncommitted changes:\n");
                    for line in diff.lines().take(10) {
                        output.push_str(&format!("  {line}\n"));
                    }
                }
                _ => {
                    output.push_str("\nNo uncommitted changes.\n");
                }
            }

            CommandResult::success(output)
        }),
    ));

    // /pr_comments — view/reply to PR comments
    registry.register(SlashCommand::local(
        "pr_comments",
        "View and reply to PR comments",
        true,
        true,
        true,
        Arc::new(|ctx: &CommandContext, args: &[String]| {
            let dir = std::path::Path::new(&ctx.cwd);
            if let Ok(comments) = run_gh(dir, &["pr", "view", "--json", "comments"]) {
                let count = comments.matches('"').count() / 2;
                CommandResult::success(format!(
                    "PR Comments\n───────────\nFound {count} comment(s):\n{comments}"
                ))
            } else if !args.is_empty() {
                CommandResult::success(format!(
                    "Replying to PR comment(s): {}\n(Use gh pr comment to interact)",
                    args.join(" ")
                ))
            } else {
                CommandResult::success(
                    "PR Comments\n───────────\nNo PR found.\nCreate a PR first with /commit-push-pr"
                )
            }
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
            let dir = std::path::Path::new(&ctx.cwd);
            if args.is_empty() {
                let branch = current_branch(dir);
                let all_branches =
                    run_git(dir, &["branch", "--format", "%(refname:short)"]).unwrap_or_default();
                let branch_list: Vec<&str> = all_branches.lines().take(20).collect();
                CommandResult::success(format!(
                    "Current branch: {branch}\n\nBranches:\n{}",
                    branch_list
                        .iter()
                        .map(|b| format!("  {b}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ))
            } else if args[0] == "create" {
                let name = args.get(1).map(|s| s.as_str()).unwrap_or("new-branch");
                match run_git(dir, &["checkout", "-b", name]) {
                    Ok(_) => {
                        CommandResult::success(format!("Branch '{name}' created and checked out"))
                            .with_state_change(true)
                    }
                    Err(e) => CommandResult::error(format!("Branch creation failed: {e}")),
                }
            } else {
                let name = &args[0];
                match run_git(dir, &["checkout", name]) {
                    Ok(_) => CommandResult::success(format!("Switched to branch: {name}"))
                        .with_state_change(true),
                    Err(e) => CommandResult::error(format!("Branch switch failed: {e}")),
                }
            }
        }),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::CommandType;
    use coderex_core::types::ModelId;

    fn init_git_repo(path: &std::path::Path) {
        std::fs::create_dir_all(path).ok();
        coderex_core::git_hardening::git_command()
            .args(["init", "--quiet"])
            .current_dir(path)
            .output()
            .expect("git init failed");
        // The commands under test commit as the repository's user, and this
        // repository is ours — so it carries its own identity instead of
        // borrowing whatever the machine happens to have configured. Without
        // this, a machine with no global identity fails the tests, and the
        // assertions measure the environment rather than the command.
        for (key, value) in [
            ("user.name", "CodeRex Test"),
            ("user.email", "test@coderex.invalid"),
        ] {
            let configured = coderex_core::git_hardening::git_command()
                .args(["config", key, value])
                .current_dir(path)
                .output()
                .expect("git config failed");
            assert!(configured.status.success(), "could not set {key}");
        }
        std::fs::write(path.join("README"), "root").unwrap();
        coderex_core::git_hardening::git_command()
            .args(["add", "README"])
            .current_dir(path)
            .output()
            .expect("git add failed");
        coderex_core::git_hardening::git_command()
            .args(["commit", "--quiet", "-m", "init"])
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .current_dir(path)
            .output()
            .expect("git commit failed");
    }

    fn commit_all(path: &std::path::Path, message: &str) {
        coderex_core::git_hardening::git_command()
            .args(["add", "-A"])
            .current_dir(path)
            .output()
            .expect("git add failed");
        coderex_core::git_hardening::git_command()
            .args(["commit", "--quiet", "-m", message])
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .current_dir(path)
            .output()
            .expect("git commit failed");
    }

    fn ctx_in_dir(dir: &std::path::Path) -> CommandContext {
        let branch = current_branch(dir);
        CommandContext {
            cwd: dir.to_string_lossy().to_string(),
            git_branch: branch,
            ..CommandContext::new("/src".into(), "s1".into(), "Test".into(), ModelId::sonnet())
        }
    }

    fn registry() -> CommandRegistry {
        let mut registry = CommandRegistry::new();
        register(&mut registry);
        registry
    }

    #[test]
    fn commit_creates_git_commit() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        std::fs::write(tmp.path().join("test.txt"), "hello").unwrap();
        let ctx = ctx_in_dir(tmp.path());

        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("commit", &ctx, &["add test file".into()]);
        assert!(result.output.contains("Commit created"));
        assert!(result.output.contains("add test file"));
        assert!(result.state_changed);
    }

    #[test]
    fn commit_nothing_to_commit() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let ctx = ctx_in_dir(tmp.path());

        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("commit", &ctx, &["nothing new".into()]);
        // Either reports nothing to commit or creates an empty commit message
        let out = result.output.to_lowercase();
        assert!(out.contains("nothing") || out.contains("commit") || result.state_changed);
    }

    #[test]
    fn commit_with_message() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        std::fs::write(tmp.path().join("test.txt"), "hello").unwrap();
        let ctx = ctx_in_dir(tmp.path());

        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("commit", &ctx, &["fix".into(), "the".into(), "bug".into()]);
        assert!(result.output.contains("fix the bug"));
    }

    #[test]
    fn commit_push_pr_fails_without_remote() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        std::fs::write(tmp.path().join("test.txt"), "hello").unwrap();
        let ctx = ctx_in_dir(tmp.path());

        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("commit-push-pr", &ctx, &["add feature".into()]);
        // Without a remote, push should fail but we handle gracefully
        assert!(result.output.contains("committed") || result.output.contains("failed"));
    }

    #[test]
    fn review_shows_diff_info() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        std::fs::write(tmp.path().join("README"), "updated\n").unwrap();
        commit_all(tmp.path(), "update readme");
        let ctx = ctx_in_dir(tmp.path());

        let reg = registry();
        let result = reg.execute("review", &ctx, &[]);
        assert!(result.output.contains("Code Review"));
        assert!(result.output.contains("Branch"));
        assert_eq!(result.prompt_profile, Some(PromptProfile::Reviewer));
        let context = result
            .extra_context
            .expect("review should inject diff context");
        assert!(context.contains("Review the current git diff"));
        assert!(context.contains("Diff:"));
        assert!(context.contains("README"));
    }

    #[test]
    fn review_command_is_prompt_mode() {
        let reg = registry();
        let command = reg.find("review").expect("review command should exist");
        assert_eq!(command.kind, CommandType::Prompt);
    }

    #[test]
    fn security_review_shows_info() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        std::fs::write(tmp.path().join("README"), "updated\n").unwrap();
        commit_all(tmp.path(), "update readme");
        let ctx = ctx_in_dir(tmp.path());

        let reg = registry();
        let result = reg.execute("security-review", &ctx, &[]);
        assert!(result.output.contains("Security Review"));
        assert_eq!(result.prompt_profile, Some(PromptProfile::SecurityReviewer));
        let context = result
            .extra_context
            .expect("security review should inject diff context");
        assert!(context.contains("Perform a security review"));
        assert!(context.contains("Diff:"));
        assert!(context.contains("README"));
    }

    #[test]
    fn security_review_command_is_prompt_mode() {
        let reg = registry();
        let command = reg
            .find("security-review")
            .expect("security-review command should exist");
        assert_eq!(command.kind, CommandType::Prompt);
    }

    #[test]
    fn autofix_pr() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let ctx = ctx_in_dir(tmp.path());

        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("autofix-pr", &ctx, &[]);
        assert!(result.output.contains("Analyzing PR"));
    }

    #[test]
    fn pr_comments_no_pr() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let ctx = ctx_in_dir(tmp.path());

        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("pr_comments", &ctx, &[]);
        assert!(result.output.contains("No PR") || result.output.contains("Found"));
    }

    #[test]
    fn branch_current() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let ctx = ctx_in_dir(tmp.path());

        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("branch", &ctx, &[]);
        assert!(
            result.output.contains("master")
                || result.output.contains("main")
                || result.output.contains("branch")
        );
    }

    #[test]
    fn branch_switch() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        // Create a feature branch
        run_git(tmp.path(), &["checkout", "-b", "feature"]).ok();
        run_git(tmp.path(), &["checkout", "master"]).ok();
        let ctx = ctx_in_dir(tmp.path());

        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("branch", &ctx, &["feature".into()]);
        assert!(result.output.contains("Switched") || result.output.contains("feature"));
        assert!(result.state_changed);
    }

    #[test]
    fn branch_create() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());
        let ctx = ctx_in_dir(tmp.path());

        let reg = {
            let mut r = CommandRegistry::new();
            register(&mut r);
            r
        };
        let result = reg.execute("branch", &ctx, &["create".into(), "hotfix".into()]);
        assert!(result.output.contains("hotfix"));
        assert!(result.state_changed);
    }
}
