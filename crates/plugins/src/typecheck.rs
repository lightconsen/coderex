//! Type-check the declarations a plugin ships.
//!
//! A plugin may declare `.d.ts` files; [`PluginManager::export_plugin_types`]
//! writes them next to the host's own generated declarations and a
//! `tsconfig.json` that covers the whole tree. That makes the files readable
//! and listable, but nothing has ever *checked* them: a plugin could ship a
//! declaration with a syntax error and the only symptom would be an editor
//! underlining it.
//!
//! This module runs a real TypeScript compiler over that directory when one is
//! available. It does not bundle one, and it does not pretend to have checked
//! anything when it could not: [`TypeCheckOutcome::Unavailable`] says exactly
//! that.
//!
//! The compiler is looked for, in order, at:
//!
//! 1. the path in `CODEREX_TSC`, when set (a host-provided compiler);
//! 2. `<root>/node_modules/.bin/tsc`, which is where a project-local install
//!    puts it;
//! 3. `tsc` on `PATH`.
//!
//! [`PluginManager::export_plugin_types`]: crate::plugins::PluginManager::export_plugin_types

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Environment variable naming a TypeScript compiler to use.
pub const TSC_ENV: &str = "CODEREX_TSC";

/// How long a check may take before it is abandoned.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// One compiler diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// File the diagnostic points at, as the compiler printed it.
    pub file: String,
    /// 1-based line.
    pub line: u32,
    /// 1-based column.
    pub column: u32,
    /// The `TS####` code.
    pub code: String,
    /// The message text.
    pub message: String,
}

impl Diagnostic {
    /// `file(line,col): TS#### message`, the shape a person reads.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{}({},{}): {} {}",
            self.file, self.line, self.column, self.code, self.message
        )
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeCheckOutcome {
    /// The compiler ran and reported nothing.
    Passed,
    /// The compiler ran and reported errors.
    Failed(Vec<Diagnostic>),
    /// No compiler was found, or it could not be started.
    Unavailable(String),
    /// The compiler did not finish within the timeout.
    TimedOut(Duration),
}

impl TypeCheckOutcome {
    /// One line a person or a log can carry.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::Passed => "type check passed".to_string(),
            Self::Failed(diagnostics) => format!(
                "{} type error(s): {}",
                diagnostics.len(),
                diagnostics
                    .iter()
                    .take(3)
                    .map(Diagnostic::render)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Self::Unavailable(reason) => format!("type check unavailable: {reason}"),
            Self::TimedOut(limit) => {
                format!("type check timed out after {}s", limit.as_secs())
            }
        }
    }

    /// Whether the check actually ran.
    #[must_use]
    pub const fn ran(&self) -> bool {
        matches!(self, Self::Passed | Self::Failed(_))
    }
}

/// Parse `tsc --pretty false` diagnostics.
///
/// Lines look like `src/index.d.ts(3,15): error TS2304: Cannot find name
/// 'Foo'.`; a compiler-level error (`error TS5058: The specified path does not
/// exist`) carries no location and is reported against the config file. Lines
/// that are neither (warnings, the "Found N errors" footer, blank lines) are
/// ignored: an error is what makes the check fail, and a warning is noise here.
#[must_use]
pub fn parse_diagnostics(output: &str) -> Vec<Diagnostic> {
    const SEPARATOR: &str = "): ";

    let mut diagnostics = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        let Some((location, rest)) = line.split_once(SEPARATOR) else {
            continue;
        };
        // `rest` is `error TS2304: message`; warnings are not what makes a
        // check fail, so they are skipped.
        let Some(rest) = rest.strip_prefix("error ") else {
            continue;
        };
        let Some((code, message)) = rest.split_once(": ") else {
            continue;
        };
        let Some(code) = code.split_whitespace().last() else {
            continue;
        };
        if !code.starts_with("TS") {
            continue;
        }
        let Some(open) = location.rfind('(') else {
            continue;
        };
        let Some(file) = location.get(..open).map(str::trim) else {
            continue;
        };
        let Some((line_number, column)) = location[open + 1..].split_once(',') else {
            continue;
        };
        let (Ok(line_number), Ok(column)) = (
            line_number.trim().parse::<u32>(),
            column.trim().parse::<u32>(),
        ) else {
            continue;
        };
        diagnostics.push(Diagnostic {
            file: file.to_string(),
            line: line_number,
            column,
            code: code.to_string(),
            message: message.trim().to_string(),
        });
    }
    diagnostics
}

/// The compilers to try, most specific first.
#[must_use]
pub fn compiler_candidates(root: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(configured) = std::env::var_os(TSC_ENV)
        && !configured.is_empty()
    {
        candidates.push(PathBuf::from(configured));
    }
    candidates.push(root.join("node_modules").join(".bin").join("tsc"));
    candidates.push(PathBuf::from("tsc"));
    candidates
}

/// Type-check the declarations under `root`.
///
/// `root` is the directory [`PluginManager::export_plugin_types`] wrote: it
/// must contain `tsconfig.json`.
///
/// [`PluginManager::export_plugin_types`]: crate::plugins::PluginManager::export_plugin_types
#[must_use]
pub fn typecheck(root: &Path, timeout: Duration) -> TypeCheckOutcome {
    typecheck_with(root, None, timeout)
}

/// Type-check with an explicit compiler, or the first one found when `None`.
///
/// The explicit form exists for tests and for hosts that resolve the compiler
/// themselves; it bypasses [`TSC_ENV`] and the search.
#[must_use]
pub fn typecheck_with(root: &Path, compiler: Option<&Path>, timeout: Duration) -> TypeCheckOutcome {
    let config = root.join("tsconfig.json");
    if !config.is_file() {
        return TypeCheckOutcome::Unavailable(format!(
            "{} has no tsconfig.json; the declarations are exported at startup",
            root.display()
        ));
    }

    let candidates = compiler.map_or_else(
        || compiler_candidates(root),
        |path| vec![path.to_path_buf()],
    );
    let mut last_error = None;
    for candidate in candidates {
        match run_compiler(&candidate, root, timeout) {
            Ok(output) => return classify(&output),
            Err(RunError::NotFound(error)) => {
                last_error = Some(format!("{}: {error}", candidate.display()));
            }
            Err(RunError::TimedOut) => return TypeCheckOutcome::TimedOut(timeout),
        }
    }
    TypeCheckOutcome::Unavailable(last_error.map_or_else(
        || format!("no TypeScript compiler found (set {TSC_ENV} or install tsc)"),
        |error| {
            format!(
                "no TypeScript compiler could be run (set {TSC_ENV} or install tsc); last attempt: {error}"
            )
        },
    ))
}

/// How running the compiler failed.
enum RunError {
    /// The program could not be started (not installed, not executable).
    NotFound(String),
    /// It started but did not finish in time.
    TimedOut,
}

/// Run `compiler --noEmit --pretty false -p tsconfig.json` in `root`.
fn run_compiler(compiler: &Path, root: &Path, timeout: Duration) -> Result<String, RunError> {
    let mut child = Command::new(compiler)
        .arg("--noEmit")
        .arg("--pretty")
        .arg("false")
        .arg("-p")
        .arg("tsconfig.json")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| RunError::NotFound(error.to_string()))?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RunError::TimedOut);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(RunError::NotFound(error.to_string())),
        }
    }

    // `wait_with_output` after a successful `try_wait` still drains the pipes.
    let output = child
        .wait_with_output()
        .map_err(|error| RunError::NotFound(error.to_string()))?;
    // tsc prints diagnostics to stdout; the config-level errors go to stderr.
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        text.push('\n');
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    if text.trim().is_empty() && !output.status.success() {
        // A non-zero exit with no text is unusual; say so rather than pass.
        text = format!("tsc exited with {}", output.status);
    }
    Ok(text)
}

/// Turn the compiler's output into an outcome.
///
/// A non-zero exit with no `TS####` diagnostic means the compiler itself
/// failed (a bad flag, a missing binary behind a shim); that is reported as
/// unavailable rather than as a clean pass, because nothing was checked.
fn classify(output: &str) -> TypeCheckOutcome {
    let diagnostics = parse_diagnostics(output);
    if !diagnostics.is_empty() {
        return TypeCheckOutcome::Failed(diagnostics);
    }
    if output.trim().is_empty() {
        return TypeCheckOutcome::Passed;
    }
    // Text with no diagnostic is a compiler-level message (`error TS5058`
    // carries no location and is skipped by the parser).
    let looks_like_failure = output
        .lines()
        .any(|line| line.contains("error TS") || line.contains("Cannot find"));
    if looks_like_failure {
        return TypeCheckOutcome::Unavailable(output.trim().to_string());
    }
    TypeCheckOutcome::Passed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_located_error_is_parsed() {
        let output = "src/index.d.ts(3,15): error TS2304: Cannot find name 'Foo'.\n";
        let diagnostics = parse_diagnostics(output);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].file, "src/index.d.ts");
        assert_eq!(diagnostics[0].line, 3);
        assert_eq!(diagnostics[0].column, 15);
        assert_eq!(diagnostics[0].code, "TS2304");
        assert_eq!(diagnostics[0].message, "Cannot find name 'Foo'.");
        assert_eq!(
            diagnostics[0].render(),
            "src/index.d.ts(3,15): TS2304 Cannot find name 'Foo'."
        );
    }

    #[test]
    fn several_errors_and_the_footer_are_parsed() {
        let output = "\
a.d.ts(1,1): error TS1005: ';' expected.
b.d.ts(2,7): error TS2307: Cannot find module './x'.

Found 2 errors in 2 files.
";
        assert_eq!(parse_diagnostics(output).len(), 2);
    }

    #[test]
    fn warnings_and_noise_are_ignored() {
        let output = "\
a.d.ts(1,1): warning TS9999: something advisory
Version 5.6.3
src/index.d.ts(3,15): error TS2304: Cannot find name 'Foo'.
";
        let diagnostics = parse_diagnostics(output);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].code, "TS2304");
    }

    #[test]
    fn a_compiler_without_a_location_is_unavailable_not_a_pass() {
        // `error TS5058` has no `(line,col)`, so the parser skips it; an empty
        // outcome would read as success.
        assert_eq!(
            classify("error TS5058: The specified path does not exist: 'tsconfig.json'.\n"),
            TypeCheckOutcome::Unavailable(
                "error TS5058: The specified path does not exist: 'tsconfig.json'.".to_string()
            )
        );
    }

    #[test]
    fn empty_output_is_a_pass() {
        assert_eq!(classify(""), TypeCheckOutcome::Passed);
        assert_eq!(classify("  \n"), TypeCheckOutcome::Passed);
    }

    #[test]
    fn a_missing_tsconfig_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = typecheck(dir.path(), Duration::from_secs(1));
        assert!(
            matches!(outcome, TypeCheckOutcome::Unavailable(_)),
            "{outcome:?}"
        );
        assert!(!outcome.ran());
    }

    /// A stand-in compiler: a shell script that prints what it is told.
    #[cfg(unix)]
    fn script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("fake-tsc");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn exported_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn a_clean_compiler_run_passes() {
        let dir = exported_dir();
        let compiler = script(dir.path(), "exit 0");
        assert_eq!(
            typecheck_with(dir.path(), Some(&compiler), Duration::from_secs(5)),
            TypeCheckOutcome::Passed
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_compiler_error_is_reported_with_its_location() {
        let dir = exported_dir();
        let compiler = script(
            dir.path(),
            "echo \"index.d.ts(2,9): error TS2304: Cannot find name 'Nope'.\"; exit 1",
        );
        let outcome = typecheck_with(dir.path(), Some(&compiler), Duration::from_secs(5));
        match &outcome {
            TypeCheckOutcome::Failed(diagnostics) => {
                assert_eq!(diagnostics.len(), 1);
                assert_eq!(diagnostics[0].code, "TS2304");
            }
            other => panic!("expected diagnostics, got {other:?}"),
        }
        assert!(outcome.summary().contains("TS2304"));
    }

    #[cfg(unix)]
    #[test]
    fn the_compiler_receives_the_expected_arguments() {
        let dir = exported_dir();
        let recorded = dir.path().join("args.txt");
        let compiler = script(
            dir.path(),
            &format!("echo \"$@\" > {}\nexit 0", recorded.display()),
        );
        let _ = typecheck_with(dir.path(), Some(&compiler), Duration::from_secs(5));
        let args = std::fs::read_to_string(&recorded).unwrap();
        assert!(args.contains("--noEmit"), "{args}");
        assert!(args.contains("--pretty false"), "{args}");
        assert!(args.contains("-p tsconfig.json"), "{args}");
    }

    #[cfg(unix)]
    #[test]
    fn a_hanging_compiler_times_out() {
        let dir = exported_dir();
        let compiler = script(dir.path(), "sleep 30");
        let outcome = typecheck_with(dir.path(), Some(&compiler), Duration::from_millis(150));
        assert!(
            matches!(outcome, TypeCheckOutcome::TimedOut(_)),
            "{outcome:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_compiler_that_cannot_start_is_unavailable() {
        let dir = exported_dir();
        let missing = dir.path().join("not-a-compiler");
        let outcome = typecheck_with(dir.path(), Some(&missing), Duration::from_secs(1));
        assert!(
            matches!(outcome, TypeCheckOutcome::Unavailable(_)),
            "{outcome:?}"
        );
        assert!(!outcome.ran());
    }
}
