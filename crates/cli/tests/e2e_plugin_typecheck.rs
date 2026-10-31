//! `coderex plugin-typecheck` against a stand-in TypeScript compiler.
//!
//! The unit tests in `coderex-plugins` cover parsing and spawning; this runs
//! the real binary so the exit status and the printed report — what a plugin
//! author or CI script actually consumes — are exercised too.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A directory that looks like the exported declarations: a `tsconfig.json`
/// and nothing else the checker needs.
fn exported_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("tsconfig.json"), "{}").expect("write tsconfig");
    dir
}

/// A compiler stand-in that prints `body` and exits with `status`.
#[cfg(unix)]
fn fake_compiler(dir: &Path, body: &str, status: i32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("fake-tsc");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\nexit {status}\n")).expect("write fake tsc");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make the fake compiler executable");
    path
}

fn run(dir: &Path, compiler: Option<&Path>) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coderex"));
    command
        .arg("plugin-typecheck")
        .arg("--dir")
        .arg(dir)
        .stdin(Stdio::null())
        .env("CODEREX_LOG", "off");
    // A compiler is named explicitly so the machine's own `tsc` (or its
    // absence) cannot change the result.
    match compiler {
        Some(path) => command.env("CODEREX_TSC", path),
        None => command.env("CODEREX_TSC", dir.join("no-such-compiler")),
    };
    command.output().expect("run coderex plugin-typecheck")
}

#[cfg(unix)]
#[test]
fn a_clean_check_exits_zero_and_says_so() {
    let dir = exported_dir();
    let compiler = fake_compiler(dir.path(), "true", 0);

    let output = run(dir.path(), Some(&compiler));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stdout: {stdout}");
    assert!(stdout.contains("type check passed"), "{stdout}");
}

#[cfg(unix)]
#[test]
fn a_declaration_error_fails_with_the_diagnostic() {
    let dir = exported_dir();
    let compiler = fake_compiler(
        dir.path(),
        "echo \"index.d.ts(4,11): error TS2304: Cannot find name 'Nope'.\"",
        1,
    );

    let output = run(dir.path(), Some(&compiler));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "stdout: {stdout}");
    assert!(stdout.contains("index.d.ts(4,11)"), "{stdout}");
    assert!(stdout.contains("TS2304"), "{stdout}");
}

/// No compiler is "nothing checked", not "failed": blaming the plugin for the
/// host's toolchain would be a lie, and a CI job that treats any non-zero exit
/// as a plugin error would fail for the wrong reason.
#[test]
fn a_missing_compiler_is_reported_but_exits_zero() {
    let dir = exported_dir();

    let output = run(dir.path(), None);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stdout: {stdout}");
    assert!(stdout.contains("nothing checked"), "{stdout}");
}

/// A directory with no `tsconfig.json` was never exported; say that instead of
/// pretending a check ran.
#[test]
fn an_unexported_directory_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");

    let output = run(dir.path(), None);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stdout: {stdout}");
    assert!(stdout.contains("tsconfig.json"), "{stdout}");
}
