//! The hidden `--internal-wasm-compile` subcommand, through the real binary.
//!
//! The CLI injects itself as the WASM compiler: a packaged binary has no
//! sibling `coderex-wasm-compile`, so the runtime re-executes `coderex
//! --internal-wasm-compile <input> <output>`. The command is handled before
//! clap parses anything, and it must actually compile — the plugin tests use a
//! separate helper binary, so this path had no coverage.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MODULE: &str = r#"(module (memory (export "memory") 1))"#;

fn write_module(dir: &Path) -> PathBuf {
    let path = dir.join("plugin.wat");
    std::fs::write(&path, MODULE).expect("write module");
    path
}

fn run_compile(input: &Path, output: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_coderex"))
        .arg("--internal-wasm-compile")
        .arg(input)
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run coderex")
}

#[test]
fn the_binary_compiles_a_module_when_asked_to_act_as_the_compiler() {
    let dir = tempfile::tempdir().expect("dir");
    let input = write_module(dir.path());
    let output = dir.path().join("plugin.cwasm");

    let result = run_compile(&input, &output);

    assert!(
        result.status.success(),
        "the compiler subcommand failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bytes = std::fs::read(&output).expect("the compiler wrote no output");
    assert!(!bytes.is_empty(), "the compiled module is empty");
    // The output is `wasmtime::Module::serialize` bytes, not text.
    assert!(
        bytes.len() > 64,
        "the output is too small to be a serialized module: {} bytes",
        bytes.len()
    );
}

/// The flag is only a compiler command as the *first* argument. Anywhere else
/// clap sees it, and a user typo must not silently compile something.
#[test]
fn the_flag_later_in_the_arguments_is_a_usage_error() {
    let dir = tempfile::tempdir().expect("dir");
    let input = write_module(dir.path());

    let result = Command::new(env!("CARGO_BIN_EXE_coderex"))
        .args(["-p", "--internal-wasm-compile"])
        .arg(&input)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run coderex");

    assert!(
        !result.status.success(),
        "a misplaced compiler flag was accepted"
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("unexpected argument") || stderr.contains("--internal-wasm-compile"),
        "the refusal does not name the flag: {stderr}"
    );
}

/// A compiler that cannot write its output reports the failure and exits
/// non-zero, rather than exiting 0 with nothing written.
#[test]
fn a_failed_compile_exits_non_zero() {
    let dir = tempfile::tempdir().expect("dir");
    let input = write_module(dir.path());
    // A directory is not a writable output file.
    let output = dir.path().join("out-dir");
    std::fs::create_dir(&output).unwrap();

    let result = run_compile(&input, &output);

    assert!(
        !result.status.success(),
        "a failed compile exited 0 (stdout: {})",
        String::from_utf8_lossy(&result.stdout)
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("coderex:"),
        "the failure was not reported: {stderr}"
    );
}
