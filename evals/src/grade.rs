//! Grading a case — what the run produced against what the case demanded.

use std::path::Path;

use crate::harness::Expectations;

/// What a graded run produced, before it is judged.
#[derive(Debug, Clone)]
pub struct RunOutput {
    /// Exit code, when the command exited on its own.
    pub exit_code: Option<i32>,
    /// Everything the command wrote, stdout then stderr.
    pub output: String,
    /// Where it ran, for the file checks.
    pub cwd: std::path::PathBuf,
    /// Whether it was stopped for taking too long.
    pub timed_out: bool,
}

/// What grading concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Whether every expectation held.
    pub passed: bool,
    /// One line per expectation that did not.
    pub failures: Vec<String>,
}

/// Judge a run against a case's expectations.
#[must_use]
pub fn grade(expect: &Expectations, run: &RunOutput) -> Verdict {
    let mut failures = Vec::new();

    if run.timed_out {
        failures.push("the command was still running when the timeout expired".to_string());
    }

    if let Some(expected) = expect.exit_code {
        match run.exit_code {
            Some(actual) if actual == expected => {}
            Some(actual) => failures.push(format!("exit code {actual}, expected {expected}")),
            None => failures.push(format!("no exit code, expected {expected}")),
        }
    }

    for needle in &expect.output_contains {
        if !run.output.contains(needle.as_str()) {
            failures.push(format!("output does not contain {needle:?}"));
        }
    }

    for needle in &expect.output_not_contains {
        if run.output.contains(needle.as_str()) {
            failures.push(format!("output contains {needle:?}"));
        }
    }

    for (path, needle) in &expect.files_contain {
        let target = run.cwd.join(path);
        match std::fs::read_to_string(&target) {
            Ok(contents) => {
                if !contents.contains(needle.as_str()) {
                    failures.push(format!("{path} does not contain {needle:?}"));
                }
            }
            Err(error) => failures.push(format!("{path} could not be read: {error}")),
        }
    }

    for path in &expect.files_absent {
        if run.cwd.join(path).exists() {
            failures.push(format!("{path} should not exist"));
        }
    }

    Verdict {
        passed: failures.is_empty(),
        failures,
    }
}

/// Whether a case asks for anything at all.
///
/// A case with no expectations passes as soon as the command runs: useful when
/// the thing being measured is cost or duration, and worth marking so a case
/// that was *meant* to assert something is not mistaken for one that did.
#[must_use]
pub fn expects_something(expect: &Expectations) -> bool {
    expect.exit_code.is_some()
        || !expect.output_contains.is_empty()
        || !expect.output_not_contains.is_empty()
        || !expect.files_contain.is_empty()
        || !expect.files_absent.is_empty()
}

/// Resolve a path written in a case against the directory it ran in.
#[must_use]
pub fn resolve_in_run(cwd: &Path, path: &str) -> Option<std::path::PathBuf> {
    let relative = crate::paths::path_safe_id(path).ok()?;
    let candidate = cwd.join(relative);
    candidate.starts_with(cwd).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn run(exit_code: Option<i32>, output: &str) -> RunOutput {
        RunOutput {
            exit_code,
            output: output.to_string(),
            cwd: std::env::temp_dir(),
            timed_out: false,
        }
    }

    #[test]
    fn a_case_with_no_expectations_passes_when_it_runs() {
        let verdict = grade(&Expectations::default(), &run(Some(0), ""));
        assert!(verdict.passed);
        assert!(!expects_something(&Expectations::default()));
    }

    #[test]
    fn the_exit_code_is_checked() {
        let expect = Expectations {
            exit_code: Some(2),
            ..Expectations::default()
        };
        assert!(grade(&expect, &run(Some(2), "")).passed);

        let wrong = grade(&expect, &run(Some(0), ""));
        assert!(!wrong.passed);
        assert!(wrong.failures[0].contains("exit code 0, expected 2"));
    }

    #[test]
    fn a_missing_exit_code_is_a_failure_when_one_was_expected() {
        let expect = Expectations {
            exit_code: Some(0),
            ..Expectations::default()
        };
        let verdict = grade(&expect, &run(None, ""));
        assert!(!verdict.passed);
        assert!(verdict.failures[0].contains("no exit code"));
    }

    #[test]
    fn output_is_checked_both_ways() {
        let expect = Expectations {
            output_contains: vec!["published".into()],
            output_not_contains: vec!["error".into()],
            ..Expectations::default()
        };

        assert!(grade(&expect, &run(Some(0), "page published")).passed);

        let missing = grade(&expect, &run(Some(0), "nothing happened"));
        assert!(missing.failures[0].contains("does not contain"));

        let forbidden = grade(&expect, &run(Some(0), "published with error"));
        assert!(forbidden.failures[0].contains("output contains"));
    }

    #[test]
    fn files_are_checked_in_the_directory_the_case_ran_in() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("out.txt"), "the answer is 42").unwrap();

        let expect = Expectations {
            files_contain: BTreeMap::from([("out.txt".to_string(), "42".to_string())]),
            files_absent: vec!["missing.txt".into()],
            ..Expectations::default()
        };

        let mut output = run(Some(0), "");
        output.cwd = dir.path().to_path_buf();
        assert!(grade(&expect, &output).passed);

        std::fs::write(dir.path().join("missing.txt"), "here now").unwrap();
        let verdict = grade(&expect, &output);
        assert!(!verdict.passed);
        assert!(verdict.failures[0].contains("should not exist"));
    }

    #[test]
    fn a_file_that_is_not_there_fails_the_case_that_expected_it() {
        let dir = tempfile::tempdir().unwrap();
        let expect = Expectations {
            files_contain: BTreeMap::from([("gone.txt".to_string(), "x".to_string())]),
            ..Expectations::default()
        };
        let mut output = run(Some(0), "");
        output.cwd = dir.path().to_path_buf();

        let verdict = grade(&expect, &output);
        assert!(!verdict.passed);
        assert!(verdict.failures[0].contains("could not be read"));
    }

    #[test]
    fn a_timeout_is_a_failure_whatever_else_happened() {
        let mut output = run(Some(0), "all good");
        output.timed_out = true;

        let verdict = grade(&Expectations::default(), &output);
        assert!(!verdict.passed);
        assert!(verdict.failures[0].contains("timeout"));
    }

    #[test]
    fn a_file_assertion_cannot_climb_out_of_the_run() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_in_run(dir.path(), "out.txt").is_some());
        assert!(resolve_in_run(dir.path(), "../escape.txt").is_none());
        assert!(resolve_in_run(dir.path(), "/etc/passwd").is_none());
    }
}
