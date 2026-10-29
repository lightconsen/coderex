//! Integration tests for out-of-process WASM compilation and the AOT cache.
//!
//! `cargo test -p coderex-plugins` builds `coderex-wasm-compile` next to the
//! test binaries and exposes it through `CARGO_BIN_EXE_*`, so these tests
//! exercise the real compiler process rather than an in-process stand-in.
//!
//! Every test takes `ENV_LOCK`: the compiler child inherits this process's
//! environment, so a counter or timeout set by one test must not be observed by
//! another test's child.
#![cfg(feature = "wasm-runtime")]
// `std::env::set_var` is unsafe on edition 2024 because it races with other
// threads reading the environment. These tests serialize every mutation behind
// `ENV_LOCK` and take the same lock for tests that only spawn children.
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use coderex_plugins::wasm_runtime::{PluginWasmInstance, WasmCompilerCommand};

const TIMEOUT_ENV: &str = "CODEREX_WASM_COMPILE_TIMEOUT_MS";
const SLEEP_ENV: &str = "CODEREX_WASM_COMPILE_TEST_SLEEP_MS";
const COUNT_ENV: &str = "CODEREX_WASM_COMPILE_TEST_COUNT_FILE";
const CHILD_PID_ENV: &str = "CODEREX_WASM_COMPILE_TEST_CHILD_PID_FILE";
const NO_SANDBOX_ENV: &str = "CODEREX_WASM_COMPILE_TEST_NO_SANDBOX";

fn set_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: callers hold `ENV_LOCK`, so no other test in this binary mutates
    // or spawns a child while the variable is set.
    unsafe { std::env::set_var(key, value) };
}

fn remove_env(key: &str) {
    // SAFETY: callers hold `ENV_LOCK`.
    unsafe { std::env::remove_var(key) };
}

/// Serializes environment mutation *and* child spawning.
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

fn helper_command() -> WasmCompilerCommand {
    WasmCompilerCommand::new(helper_binary())
}

/// A `memory` export is enough for the runtime to accept and compile a module.
const NORMAL_MODULE: &str = r#"(module (memory (export "memory") 1))"#;

fn load_with(
    input: &Path,
    compiler: Option<WasmCompilerCommand>,
    cache: &Path,
) -> Result<PluginWasmInstance, String> {
    PluginWasmInstance::load_with(input, compiler, Some(cache.to_path_buf()))
}

/// The helper compiles and serializes a module; the parent's deserializer
/// accepts the bytes. The same module then loads through the injected-command
/// path (`program`, no leading args), proving compile -> serialize ->
/// deserialize end to end.
#[test]
fn injected_command_compiles_serializes_and_deserializes() {
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

    let cache = tempfile::tempdir().unwrap();
    load_with(&input, Some(helper_command()), cache.path())
        .expect("the parent must load a module compiled by the injected command");
}

/// The host-injected command may carry leading arguments. A wrapper script
/// receives `--marker <input> <output>`, drops the marker, and runs the real
/// helper with the remaining pair.
#[cfg(all(debug_assertions, unix))]
#[test]
fn injected_command_forwards_leading_args() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = env_guard();
    let dir = tempfile::tempdir().unwrap();
    let input = write_module(dir.path(), "plugin.wat", NORMAL_MODULE);
    let cache = tempfile::tempdir().unwrap();

    let script = dir.path().join("compile.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nshift\nexec '{}' \"$@\"\n", helper_binary()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    // The compiler sandbox intentionally only executes the compiler binary,
    // not scripts; disable it for this wrapper test.
    set_env(NO_SANDBOX_ENV, "1");
    let command = WasmCompilerCommand::new(&script).arg("--marker");
    let result = load_with(&input, Some(command), cache.path());
    remove_env(NO_SANDBOX_ENV);

    result.expect("the injected command's leading arguments must be forwarded");
}

/// A host that configures the compiler through the public setter (the path the
/// CLI takes) gets a working load. `CODEREX_WASM_CACHE_DIR` keeps the entry out
/// of the real user cache.
#[test]
fn host_injected_global_command_is_used() {
    let _guard = env_guard();
    let dir = tempfile::tempdir().unwrap();
    let input = write_module(dir.path(), "plugin.wat", NORMAL_MODULE);
    let cache = tempfile::tempdir().unwrap();

    coderex_plugins::wasm_runtime::set_compiler_command(helper_command());
    set_env("CODEREX_WASM_CACHE_DIR", cache.path());
    let result = PluginWasmInstance::new(&input);
    remove_env("CODEREX_WASM_CACHE_DIR");

    result.expect("a host-injected compiler command must be used");
}

/// With no compiler command available, loading fails closed with an actionable
/// message and never compiles in-process.
#[test]
fn missing_compiler_command_fails_closed() {
    let _guard = env_guard();
    let dir = tempfile::tempdir().unwrap();
    let input = write_module(dir.path(), "plugin.wat", NORMAL_MODULE);
    let cache = tempfile::tempdir().unwrap();

    let err = load_with(&input, None, cache.path())
        .expect_err("a plugin must not load without a compiler");

    assert!(
        err.contains("no WASM compiler command"),
        "expected the fail-closed explanation, got: {err}"
    );
    assert!(
        err.contains("set_compiler_command"),
        "the error must tell the host how to fix it, got: {err}"
    );
    assert!(
        err.contains("in-process"),
        "the error must say in-process compilation is refused, got: {err}"
    );
    assert_eq!(
        std::fs::read_dir(cache.path()).unwrap().count(),
        0,
        "the failed load must not leave cache or staging artifacts"
    );
}

/// A helper that never finishes is killed at the wall-clock limit, and the
/// descendant it spawned is killed with it (process-group kill), rather than
/// surviving as an orphan.
#[cfg(all(debug_assertions, unix))]
#[test]
fn slow_helper_is_killed_with_no_orphan_tree() {
    let _guard = env_guard();
    let dir = tempfile::tempdir().unwrap();
    let input = write_module(dir.path(), "plugin.wat", NORMAL_MODULE);
    let cache = tempfile::tempdir().unwrap();
    let child_pid_file = dir.path().join("child.pid");

    set_env(TIMEOUT_ENV, "300");
    set_env(SLEEP_ENV, "30000");
    set_env(CHILD_PID_ENV, &child_pid_file);
    // The compiler sandbox only lets the helper write its staging directory;
    // this test needs the helper to record a pid in the test's temp dir. The
    // process-group kill under test is independent of the filesystem sandbox.
    set_env(NO_SANDBOX_ENV, "1");

    let started = std::time::Instant::now();
    let err = load_with(&input, Some(helper_command()), cache.path())
        .expect_err("a helper that sleeps must not make the parent wait forever");
    let elapsed = started.elapsed();

    remove_env(TIMEOUT_ENV);
    remove_env(SLEEP_ENV);
    remove_env(CHILD_PID_ENV);
    remove_env(NO_SANDBOX_ENV);

    assert!(
        err.contains("wall-clock") && err.contains("killed"),
        "expected a killed-on-timeout error, got: {err}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the parent should have killed the helper promptly, took {elapsed:?}"
    );

    let pid: i32 = std::fs::read_to_string(&child_pid_file)
        .expect("the helper did not record its child's pid")
        .trim()
        .parse()
        .expect("bad child pid");
    assert!(
        wait_until_process_gone(pid, std::time::Duration::from_secs(3)),
        "descendant pid {pid} survived the timeout kill"
    );
}

/// The Windows arm of the orphan test: the compiler and its descendant are in
/// one Job Object, and terminating the job (the timeout path) takes both.
///
/// The sandbox stays on. The runtime adds the pid file's directory to the
/// policy instead of opting out, so this exercises the confined path.
#[cfg(all(debug_assertions, windows))]
#[test]
fn slow_helper_is_killed_with_no_orphan_tree() {
    let _guard = env_guard();
    let dir = tempfile::tempdir().unwrap();
    let input = write_module(dir.path(), "plugin.wat", NORMAL_MODULE);
    let cache = tempfile::tempdir().unwrap();
    let child_pid_file = dir.path().join("child.pid");

    // An AppContainer start plus wasmtime initialisation can take longer than
    // the Unix 300 ms budget, which would kill the helper before its test hook
    // records the descendant's pid. Three seconds is still well under the
    // "prompt kill" assertion below.
    set_env(TIMEOUT_ENV, "3000");
    set_env(SLEEP_ENV, "30000");
    set_env(CHILD_PID_ENV, &child_pid_file);

    let started = std::time::Instant::now();
    let err = load_with(&input, Some(helper_command()), cache.path())
        .expect_err("a helper that sleeps must not make the parent wait forever");
    let elapsed = started.elapsed();

    remove_env(TIMEOUT_ENV);
    remove_env(SLEEP_ENV);
    remove_env(CHILD_PID_ENV);

    assert!(
        err.contains("wall-clock") && err.contains("killed"),
        "expected a killed-on-timeout error, got: {err}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the parent should have killed the helper promptly, took {elapsed:?}"
    );

    let recorded = std::fs::read_to_string(&child_pid_file)
        .expect("the helper did not record its child's pid");
    let pid: u32 = recorded
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("bad child pid record: {recorded:?}"));
    assert!(
        wait_until_process_gone_windows(pid, std::time::Duration::from_secs(5)),
        "descendant pid {pid} survived the timeout kill"
    );
}

/// Poll until `Get-Process -Id` can no longer find the pid.
#[cfg(all(debug_assertions, windows))]
fn wait_until_process_gone_windows(pid: u32, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    let script = format!(
        "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"
    );
    while std::time::Instant::now() < deadline {
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if matches!(status, Ok(status) if !status.success()) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// Poll `kill(pid, 0)` until the process is gone or the budget expires.
#[cfg(all(debug_assertions, unix))]
fn wait_until_process_gone(pid: i32, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        // SAFETY: signal 0 performs only the existence/permission check.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// The second load finds the module in the AOT cache and does not invoke the
/// compiler again.
#[cfg(debug_assertions)]
#[test]
fn second_load_hits_the_aot_cache() {
    let _guard = env_guard();
    let dir = tempfile::tempdir().unwrap();
    let input = write_module(dir.path(), "plugin.wat", NORMAL_MODULE);
    let cache = tempfile::tempdir().unwrap();
    let count_file = dir.path().join("compiles");

    set_env(COUNT_ENV, &count_file);
    // The counter lives outside the compiler's staging directory, which the
    // macOS sandbox would not let the helper write; the cache behaviour under
    // test does not depend on the sandbox.
    set_env(NO_SANDBOX_ENV, "1");
    load_with(&input, Some(helper_command()), cache.path()).expect("the first load must compile");
    load_with(&input, Some(helper_command()), cache.path()).expect("the second load must succeed");
    remove_env(COUNT_ENV);
    remove_env(NO_SANDBOX_ENV);

    let invocations = std::fs::read_to_string(&count_file).unwrap_or_default();
    assert_eq!(
        invocations.lines().count(),
        1,
        "the second load must hit the AOT cache instead of recompiling: {invocations:?}"
    );
    assert!(
        cache_contains_cwasm(cache.path()),
        "the first load must have written a .cwasm entry"
    );
}

/// Recursively look for a `.cwasm` file.
#[cfg(debug_assertions)]
fn cache_contains_cwasm(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            cache_contains_cwasm(&path)
        } else {
            path.extension().is_some_and(|ext| ext == "cwasm")
        }
    })
}

/// The pre-scan is still the first gate: a pathological module is rejected
/// before the compiler process is started at all.
#[test]
fn prescan_rejects_pathological_module_before_compiling() {
    let _guard = env_guard();
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let count_file = dir.path().join("compiles");

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

    set_env(COUNT_ENV, &count_file);
    let err = load_with(&input, Some(helper_command()), cache.path()).unwrap_err();
    remove_env(COUNT_ENV);

    assert!(
        err.contains("PLUGIN_MAX_NESTING_DEPTH"),
        "expected the pre-scan to name its limit, got: {err}"
    );
    assert!(
        !count_file.exists(),
        "the compiler must not be invoked for a module the pre-scan rejected"
    );
}

/// The compiler child gets a minimal environment: the host's variables — an
/// API key, say — do not leak into it. The wrapper dumps what it actually
/// received, then runs the real helper so the compile still succeeds.
///
/// Unix-only: the wrapper is a `sh` script made executable with `set_mode`.
#[cfg(unix)]
#[test]
fn the_compiler_child_does_not_inherit_host_secrets() {
    let _guard = env_guard();
    let dir = tempfile::tempdir().unwrap();
    let input = write_module(dir.path(), "plugin.wat", NORMAL_MODULE);
    let dump = dir.path().join("child.env");
    let wrapper = dir.path().join("dump-env.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nenv > {}\nexec {} \"$@\"\n",
            dump.display(),
            helper_binary()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&wrapper).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&wrapper, permissions).unwrap();
    }

    set_env("CODEREX_TEST_HOST_SECRET", "hunter2-should-not-leak");
    set_env(NO_SANDBOX_ENV, "1");
    let result = load_with(
        &input,
        Some(
            WasmCompilerCommand::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "env > {}; exec {} \"$@\"",
                    dump.display(),
                    helper_binary()
                ))
                .arg("sh"),
        ),
        dir.path(),
    );
    remove_env("CODEREX_TEST_HOST_SECRET");
    remove_env(NO_SANDBOX_ENV);

    result.expect("the wrapper compiles through the real helper");
    let observed = std::fs::read_to_string(&dump).expect("the wrapper dumped its environment");
    assert!(
        !observed.contains("hunter2-should-not-leak"),
        "a host variable reached the compiler child:\n{observed}"
    );
    assert!(
        observed.lines().any(|line| line.starts_with("PATH=")),
        "PATH is the one variable the compiler needs:\n{observed}"
    );
}
