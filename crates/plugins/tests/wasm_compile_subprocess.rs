//! Integration tests for out-of-process WASM compilation.
//!
//! `cargo test -p coderex-plugins` builds `coderex-wasm-compile` next to the
//! test binaries and exposes it through `CARGO_BIN_EXE_*`, so these tests
//! exercise the real helper rather than an in-process stand-in.
#![cfg(feature = "wasm-runtime")]
// `std::env::set_var` is unsafe on edition 2024 because it races with other
// threads reading the environment. These tests serialize every mutation behind
// `ENV_LOCK`; no other test in this integration binary reads those variables
// without the lock.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn set_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: callers hold `ENV_LOCK`, and the parent process's other threads
    // do not touch these variables.
    unsafe { std::env::set_var(key, value) };
}

fn remove_env(key: &str) {
    // SAFETY: callers hold `ENV_LOCK`.
    unsafe { std::env::remove_var(key) };
}

/// The compiler lookup honours a debug-only environment override. Tests that
/// change it take this lock so they cannot observe each other's values.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write_module(dir: &Path, name: &str, wat: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, wat).expect("failed to write WAT module");
    path
}

const fn helper_binary() -> &'static str {
    env!("CARGO_BIN_EXE_coderex-wasm-compile")
}

/// A `memory` export is enough for the runtime to accept and compile a module.
const NORMAL_MODULE: &str = r#"(module (memory (export "memory") 1))"#;

/// The helper compiles and serializes a module; the parent's deserializer
/// accepts the bytes. Then the full parent path is exercised with the helper
/// selected explicitly, proving compile -> serialize -> deserialize end to end.
#[test]
fn helper_compiles_serializes_and_parent_deserializes() {
    let _guard = env_guard();
    let dir = tempfile::tempdir().unwrap();
    let input = write_module(dir.path(), "plugin.wat", NORMAL_MODULE);
    let output = dir.path().join("plugin.cwasm");

    let status = std::process::Command::new(helper_binary())
        .arg(&input)
        .arg(&output)
        .status()
        .expect("failed to spawn coderex-wasm-compile");
    assert!(status.success(), "helper exited with {status}");

    let serialized = std::fs::read(&output).expect("helper did not write an output file");
    assert!(!serialized.is_empty(), "serialized module is empty");

    coderex_plugins::wasm_runtime::deserialize_compiled_module(&serialized)
        .expect("the parent must be able to deserialize the helper's output");

    set_env("CODEREX_WASM_COMPILE_HELPER", helper_binary());
    let result = coderex_plugins::wasm_runtime::PluginWasmInstance::new(&input);
    remove_env("CODEREX_WASM_COMPILE_HELPER");

    result.expect("the parent must load a module compiled by the helper");
}

/// A helper that never finishes is killed at the wall-clock limit instead of
/// pinning the caller forever.
///
/// The real helper binary is used, with its debug-only test hook making it
/// sleep before compiling. This exercises the actual `pre_exec` path, not a
/// stand-in script.
#[cfg(debug_assertions)]
#[test]
fn slow_helper_is_killed_at_the_wall_clock_limit() {
    let _guard = env_guard();
    let dir = tempfile::tempdir().unwrap();
    let input = write_module(dir.path(), "plugin.wat", NORMAL_MODULE);

    set_env("CODEREX_WASM_COMPILE_HELPER", helper_binary());
    set_env("CODEREX_WASM_COMPILE_TIMEOUT_MS", "300");
    set_env("CODEREX_WASM_COMPILE_TEST_SLEEP_MS", "30000");

    let started = std::time::Instant::now();
    let err = coderex_plugins::wasm_runtime::PluginWasmInstance::new(&input)
        .expect_err("a helper that sleeps must not make the parent wait forever");
    let elapsed = started.elapsed();

    remove_env("CODEREX_WASM_COMPILE_HELPER");
    remove_env("CODEREX_WASM_COMPILE_TIMEOUT_MS");
    remove_env("CODEREX_WASM_COMPILE_TEST_SLEEP_MS");

    assert!(
        err.contains("wall-clock") && err.contains("killed"),
        "expected a killed-on-timeout error, got: {err}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the parent should have killed the helper promptly, took {elapsed:?}"
    );
}

/// Missing helper: compilation falls back in-process and still succeeds.
#[cfg(debug_assertions)]
#[test]
fn missing_helper_falls_back_to_in_process_compilation() {
    let _guard = env_guard();
    let dir = tempfile::tempdir().unwrap();
    let input = write_module(dir.path(), "plugin.wat", NORMAL_MODULE);

    set_env(
        "CODEREX_WASM_COMPILE_HELPER",
        dir.path().join("does-not-exist"),
    );
    let result = coderex_plugins::wasm_runtime::PluginWasmInstance::new(&input);
    remove_env("CODEREX_WASM_COMPILE_HELPER");

    result.expect("the in-process fallback must compile a normal module");
}

/// The pre-scan floor still applies on the in-process fallback path.
#[cfg(debug_assertions)]
#[test]
fn fallback_still_enforces_prescan_limits() {
    let _guard = env_guard();
    let dir = tempfile::tempdir().unwrap();

    // 300 nested blocks exceeds PLUGIN_MAX_NESTING_DEPTH (256).
    let mut wat = String::from("(module (func ");
    for _ in 0..300 {
        wat.push_str("block ");
    }
    wat.push_str("nop ");
    for _ in 0..300 {
        wat.push_str("end ");
    }
    wat.push_str("))");
    let input = write_module(dir.path(), "deep.wat", &wat);

    set_env(
        "CODEREX_WASM_COMPILE_HELPER",
        dir.path().join("does-not-exist"),
    );
    let err = coderex_plugins::wasm_runtime::PluginWasmInstance::new(&input).unwrap_err();
    remove_env("CODEREX_WASM_COMPILE_HELPER");

    assert!(
        err.contains("PLUGIN_MAX_NESTING_DEPTH"),
        "the fallback must still run the pre-scan, got: {err}"
    );
}
