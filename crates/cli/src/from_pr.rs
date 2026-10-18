//! Pull-request context for `--from-pr`.
//!
//! The GitHub CLI is the credential holder: it already knows about
//! `GH_TOKEN`, the keyring, and enterprise hosts. This module shells out to
//! `gh pr view` and `gh pr diff` rather than speaking the GitHub API itself,
//! and turns the answers into one bounded block that is prepended to the
//! initial prompt.
//!
//! Failure is never degraded to "carry on without context". A missing `gh`,
//! an unauthenticated `gh`, a malformed URL, or a failed fetch is returned to
//! the caller, which stops startup with that message.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fmt::Write as _;

/// Maximum characters of the PR body kept in the context block.
const MAX_BODY_CHARS: usize = 4_000;
/// Maximum characters of the diff kept in the context block.
const MAX_DIFF_CHARS: usize = 60_000;
/// Maximum number of changed files itemised.
const MAX_FILES: usize = 200;
/// Maximum number of commit subjects listed.
const MAX_COMMITS: usize = 100;

/// A fetched pull request, rendered as one prompt-ready block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestContext {
    /// The URL the context came from.
    pub url: String,
    /// The bounded `<pr_context>` block.
    pub block: String,
}

/// Validate that `url` names a pull request.
///
/// The host is not pinned to `github.com`: `gh` also serves GitHub Enterprise
/// and a hard-coded host would make `--from-pr` useless there. What must hold
/// is the PR path shape, `<owner>/<repo>/pull/<number>`.
///
/// # Errors
///
/// Returns a message describing the expected shape when the URL cannot be a
/// pull request.
pub fn validate_pr_url(url: &str) -> Result<()> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        bail!(
            "--from-pr requires a pull request URL; expected \
             https://github.com/<owner>/<repo>/pull/<number>"
        );
    }
    let Some(rest) = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
    else {
        bail!(
            "--from-pr expects a URL like https://github.com/<owner>/<repo>/pull/<number>, \
             got `{trimmed}`"
        );
    };

    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let mut segments = rest.trim_end_matches('/').split('/');
    let host = segments.next().unwrap_or_default();
    let owner = segments.next().unwrap_or_default();
    let repo = segments.next().unwrap_or_default();
    let pull = segments.next().unwrap_or_default();
    let number = segments.next().unwrap_or_default();

    if host.is_empty()
        || owner.is_empty()
        || repo.is_empty()
        || pull != "pull"
        || number.is_empty()
        || !number.chars().all(|c| c.is_ascii_digit())
    {
        bail!(
            "--from-pr expects a URL like https://github.com/<owner>/<repo>/pull/<number>, \
             got `{trimmed}`"
        );
    }
    Ok(())
}

/// Fetch a PR and render its bounded context block.
///
/// Uses `gh pr view <url> --json title,body,files,commits` for metadata and
/// `gh pr diff <url>` for the patch. The process environment is inherited, so
/// `GH_TOKEN` (or a `gh auth login` session) is honoured by `gh` itself.
///
/// # Errors
///
/// Returns a descriptive error when the URL is not a pull request, `gh` is not
/// installed, `gh` is not authenticated, either command fails, or the JSON
/// cannot be understood.
pub fn fetch(url: &str) -> Result<PullRequestContext> {
    let url = url.trim();
    validate_pr_url(url)?;

    let view_json = run_gh(&["pr", "view", url, "--json", "title,body,files,commits"])?;
    let diff = run_gh(&["pr", "diff", url])?;

    let view: PrView = serde_json::from_str(&view_json)
        .with_context(|| format!("could not parse `gh pr view` output for {url}"))?;

    Ok(PullRequestContext {
        url: url.to_string(),
        block: render_block(url, &view, &diff),
    })
}

/// Run `gh` and return stdout, turning the usual failures into instructions.
fn run_gh(args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("gh")
        .args(args)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "the GitHub CLI `gh` was not found on PATH; install it from \
                     https://cli.github.com and sign in, or omit --from-pr"
                )
            } else {
                anyhow::anyhow!("could not run `gh {}`: {error}", args.join(" "))
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            format!("exit status {}: {stderr}", output.status)
        };
        let hint = auth_hint(&stderr);
        bail!("`gh {}` failed ({detail}){hint}", args.join(" "));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Add sign-in guidance when the failure looks like an auth problem.
fn auth_hint(stderr: &str) -> &'static str {
    let lower = stderr.to_ascii_lowercase();
    let looks_like_auth = lower.contains("auth")
        || lower.contains("not logged")
        || lower.contains("login")
        || lower.contains("gh_token")
        || lower.contains("credentials");
    if looks_like_auth {
        " — run `gh auth login`, set GH_TOKEN, or omit --from-pr"
    } else {
        ""
    }
}

/// The subset of `gh pr view --json` this module reads.
#[derive(Debug, Deserialize)]
struct PrView {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    files: Vec<PrFile>,
    #[serde(default)]
    commits: Vec<PrCommit>,
}

#[derive(Debug, Deserialize)]
struct PrFile {
    #[serde(default)]
    path: String,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
}

#[derive(Debug, Deserialize)]
struct PrCommit {
    #[serde(default)]
    oid: String,
    #[serde(default, alias = "messageHeadline")]
    headline: String,
}

/// Render the block. Every variable-length part is capped so one enormous PR
/// cannot crowd the model's context out.
fn render_block(url: &str, view: &PrView, diff: &str) -> String {
    let mut block = String::new();
    // Writing into a String cannot fail; the `let _` keeps clippy quiet
    // about the unused `Result` without unwrapping in rendering code.
    let _ = writeln!(block, "<pr_context url=\"{url}\">");
    let _ = writeln!(
        block,
        "Pull request: {}\n",
        if view.title.trim().is_empty() {
            "(untitled)"
        } else {
            view.title.trim()
        }
    );

    let body = view.body.as_deref().unwrap_or("").trim();
    block.push_str("Description:\n");
    if body.is_empty() {
        block.push_str("(no description)\n");
    } else {
        let (body, truncated) = truncate_chars(body, MAX_BODY_CHARS);
        block.push_str(&body);
        if truncated {
            block.push_str("\n[description truncated]");
        }
        block.push('\n');
    }
    block.push('\n');

    let additions: u64 = view.files.iter().map(|file| file.additions).sum();
    let deletions: u64 = view.files.iter().map(|file| file.deletions).sum();
    let _ = writeln!(
        block,
        "Changed files ({} files, +{additions}/-{deletions}):",
        view.files.len()
    );
    if view.files.is_empty() {
        block.push_str("(none reported)\n");
    } else {
        for file in view.files.iter().take(MAX_FILES) {
            let _ = writeln!(
                block,
                "- {} (+{} -{})",
                if file.path.is_empty() {
                    "(unknown)"
                } else {
                    file.path.as_str()
                },
                file.additions,
                file.deletions
            );
        }
        if view.files.len() > MAX_FILES {
            let _ = writeln!(
                block,
                "[showing {MAX_FILES} of {} changed files]",
                view.files.len()
            );
        }
    }
    block.push('\n');

    if !view.commits.is_empty() {
        let _ = writeln!(block, "Commits ({}):", view.commits.len());
        for commit in view.commits.iter().take(MAX_COMMITS) {
            let short = commit.oid.get(..7).unwrap_or(commit.oid.as_str());
            let headline = if commit.headline.trim().is_empty() {
                "(no subject)"
            } else {
                commit.headline.trim()
            };
            let _ = writeln!(block, "- {short} {headline}");
        }
        if view.commits.len() > MAX_COMMITS {
            let _ = writeln!(
                block,
                "[showing {MAX_COMMITS} of {} commits]",
                view.commits.len()
            );
        }
        block.push('\n');
    }

    block.push_str("Diff:\n");
    if diff.trim().is_empty() {
        block.push_str("(empty diff)\n");
    } else {
        let (diff, truncated) = truncate_chars(diff.trim_end(), MAX_DIFF_CHARS);
        block.push_str(&diff);
        if truncated {
            block.push_str("\n[diff truncated]");
        }
        block.push('\n');
    }
    block.push_str("</pr_context>");
    block
}

/// Split at a character boundary, reporting whether anything was dropped.
fn truncate_chars(text: &str, max: usize) -> (String, bool) {
    if text.chars().count() <= max {
        return (text.to_string(), false);
    }
    let cut = text
        .char_indices()
        .nth(max)
        .map_or(text.len(), |(index, _)| index);
    (text[..cut].to_string(), true)
}

#[cfg(all(test, unix))]
#[allow(unsafe_code)] // PATH mutation is unsafe in edition 2024; ENV_LOCK guards it.
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// Restores `PATH` when the test finishes, even on panic.
    struct PathGuard {
        previous: Option<OsString>,
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            // SAFETY: every test that changes PATH holds `ENV_LOCK`, so no
            // other test can be reading or writing it concurrently.
            unsafe {
                match self.previous.take() {
                    Some(path) => std::env::set_var("PATH", path),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }

    fn install_mock_gh(dir: &Path, script: &str) -> PathBuf {
        let path = dir.join("gh");
        std::fs::write(&path, script).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn put_on_path(dir: &Path) -> PathGuard {
        let previous = std::env::var_os("PATH");
        let path = previous.clone().map_or_else(
            || OsString::from(dir),
            |existing| {
                let mut paths = vec![dir.to_path_buf()];
                paths.extend(std::env::split_paths(&existing));
                std::env::join_paths(paths).unwrap()
            },
        );
        // SAFETY: caller holds `ENV_LOCK`.
        unsafe { std::env::set_var("PATH", path) };
        PathGuard { previous }
    }

    fn with_mock_gh(script: &str, test: impl FnOnce(&Path)) {
        let _guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        install_mock_gh(dir.path(), script);
        let _path = put_on_path(dir.path());
        test(dir.path());
    }

    const GOOD_GH: &str = r#"#!/bin/sh
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf '%s\n' '{"title":"Add widgets","body":"This adds widgets.","files":[{"path":"src/widget.rs","additions":12,"deletions":3},{"path":"README.md","additions":2,"deletions":0}],"commits":[{"oid":"abcdef1234567","messageHeadline":"Add widget support"}]}'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "diff" ]; then
  printf '%s\n' 'diff --git a/src/widget.rs b/src/widget.rs'
  printf '%s\n' '+pub fn widget() {}'
  exit 0
fi
echo "unexpected args: $*" >&2
exit 2
"#;

    #[test]
    fn valid_pull_request_urls_are_accepted() {
        for url in [
            "https://github.com/owner/repo/pull/42",
            "http://github.example.com/team/project/pull/7/",
            "https://github.com/owner/repo/pull/9?diff=split",
        ] {
            validate_pr_url(url).unwrap_or_else(|e| panic!("{url} should be valid: {e}"));
        }
    }

    #[test]
    fn invalid_urls_are_rejected_with_the_expected_shape() {
        for url in [
            "",
            "not-a-url",
            "https://github.com/owner/repo",
            "https://github.com/owner/repo/issues/12",
            "https://github.com/owner/repo/pull/not-a-number",
        ] {
            let error = validate_pr_url(url).expect_err(url);
            assert!(error.to_string().contains("pull/<number>"), "{error}");
        }
    }

    #[test]
    fn fetch_builds_a_bounded_context_block_from_gh() {
        with_mock_gh(GOOD_GH, |_dir| {
            let context = fetch("https://github.com/owner/repo/pull/42").unwrap();
            assert_eq!(context.url, "https://github.com/owner/repo/pull/42");
            assert!(context.block.contains("Add widgets"), "{}", context.block);
            assert!(
                context.block.contains("This adds widgets."),
                "{}",
                context.block
            );
            assert!(
                context.block.contains("src/widget.rs (+12 -3)"),
                "{}",
                context.block
            );
            assert!(
                context.block.contains("+pub fn widget()"),
                "{}",
                context.block
            );
            assert!(
                context.block.contains("Add widget support"),
                "{}",
                context.block
            );
            assert!(
                context.block.starts_with("<pr_context"),
                "{}",
                context.block
            );
            assert!(
                context.block.ends_with("</pr_context>"),
                "{}",
                context.block
            );
        });
    }

    #[test]
    fn fetch_reports_an_unauthenticated_gh() {
        let script = r#"#!/bin/sh
echo "gh: authentication required. Run gh auth login." >&2
exit 1
"#;
        with_mock_gh(script, |_dir| {
            let error = fetch("https://github.com/owner/repo/pull/42").unwrap_err();
            let rendered = error.to_string();
            assert!(rendered.contains("authentication required"), "{rendered}");
            assert!(rendered.contains("gh auth login"), "{rendered}");
        });
    }

    #[test]
    fn fetch_reports_a_missing_gh() {
        let _guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let empty = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("PATH");
        // SAFETY: `ENV_LOCK` is held for the whole test.
        unsafe { std::env::set_var("PATH", empty.path()) };
        let error = fetch("https://github.com/owner/repo/pull/42").unwrap_err();
        // SAFETY: as above.
        unsafe {
            match previous {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }
        let rendered = error.to_string();
        assert!(rendered.contains("not found on PATH"), "{rendered}");
        assert!(rendered.contains("cli.github.com"), "{rendered}");
    }

    #[test]
    fn fetch_reports_a_diff_failure_instead_of_returning_partial_context() {
        let script = r#"#!/bin/sh
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  printf '%s\n' '{"title":"T","body":"B","files":[],"commits":[]}'
  exit 0
fi
echo "could not find pull request" >&2
exit 1
"#;
        with_mock_gh(script, |_dir| {
            let error = fetch("https://github.com/owner/repo/pull/42").unwrap_err();
            assert!(
                error.to_string().contains("could not find pull request"),
                "{error}"
            );
        });
    }

    #[test]
    fn oversized_parts_are_truncated() {
        let view = PrView {
            title: "T".into(),
            body: Some("x".repeat(MAX_BODY_CHARS + 10)),
            files: (0..MAX_FILES + 5)
                .map(|index| PrFile {
                    path: format!("file{index}.rs"),
                    additions: 1,
                    deletions: 0,
                })
                .collect(),
            commits: Vec::new(),
        };
        let block = render_block("https://example.test/o/r/pull/1", &view, "d");
        assert!(block.contains("[description truncated]"), "{block}");
        assert!(
            block.contains(&format!(
                "[showing {MAX_FILES} of {} changed files]",
                MAX_FILES + 5
            )),
            "{block}"
        );
    }
}
