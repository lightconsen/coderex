//! WASM plugin runtime — loads and executes WebAssembly plugin modules.
//!
//! Plugin WASM modules are expected to export:
//! - `memory` — a WebAssembly memory
//! - `alloc(size: i32) -> i32` — allocate guest memory
//! - `dealloc(ptr: i32, size: i32)` — free guest memory
//! - `coderex_initialize(config_ptr: i32, config_len: i32) -> i32`
//! - `coderex_commands_list() -> i32`
//! - `coderex_skills_list() -> i32`
//! - `coderex_command_execute(name_ptr: i32, name_len: i32, args_ptr: i32, args_len: i32) -> i32`
//! - `coderex_skill_execute(name_ptr: i32, name_len: i32, ctx_ptr: i32, ctx_len: i32) -> i32`
//!
//! All pointer returns point to a null-terminated UTF-8 JSON string in guest
//! memory. The host reads the string, then deallocates it.

// The compile subprocess needs `pre_exec` to install rlimits between fork and
// exec, and wasmtime 20's `Module::deserialize` is an `unsafe fn` by design
// (it loads precompiled code). Both uses are confined to `#[cfg(unix)]` /
// deserialization sites with the safety arguments spelled out there.
#![allow(unsafe_code)]

use crate::runtime::{PluginCommandInfo, PluginSkillInfo};

/// Fuel granted to each plugin call.
///
/// Fuel is charged per WebAssembly instruction, so this bounds total guest
/// execution: a plugin that loops forever (or otherwise runs away) traps with
/// "all fuel consumed by WebAssembly" instead of pinning the host thread. The
/// budget is deliberately generous — it is a runaway guard, not a performance
/// budget.
#[cfg(feature = "wasm-runtime")]
const PLUGIN_FUEL: u64 = 100_000_000;

/// Maximum guest linear memory, in bytes.
///
/// Enforced by a `StoreLimits` resource limiter on both instantiation (the
/// module's declared minimum memory) and every `memory.grow` at runtime, so a
/// plugin cannot allocate without bound.
#[cfg(feature = "wasm-runtime")]
const PLUGIN_MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// Maximum size of a plugin `.wasm` file, in bytes (16 MiB).
///
/// `PluginWasmInstance::new` checks the file length and refuses anything over
/// this cap before reading it, and checks again after reading. This bounds the
/// input; [`PLUGIN_MAX_CODE_SECTION_BYTES`] and the other pre-scan limits bound
/// how much work the module can demand, and (when the helper is present)
/// compilation runs in a child process with hard CPU/memory limits.
#[cfg(feature = "wasm-runtime")]
const PLUGIN_MAX_WASM_BYTES: usize = 16 * 1024 * 1024;

/// Maximum bytes in the module's code section (8 MiB).
///
/// The code section is what wasmtime spends compilation time on. The file-size
/// cap above bounds the whole module including data segments; this cap bounds
/// the part that drives compilation, and is checked from the linear pre-scan
/// before `Module::new`.
#[cfg(feature = "wasm-runtime")]
const PLUGIN_MAX_CODE_SECTION_BYTES: usize = 8 * 1024 * 1024;

/// Maximum number of functions declared by a module (50,000).
#[cfg(feature = "wasm-runtime")]
const PLUGIN_MAX_FUNCTIONS: usize = 50_000;

/// Maximum total number of WebAssembly operators across all function bodies
/// (1,000,000).
#[cfg(feature = "wasm-runtime")]
const PLUGIN_MAX_OPERATORS: usize = 1_000_000;

/// Maximum `block`/`loop`/`if` nesting depth (256 levels).
#[cfg(feature = "wasm-runtime")]
const PLUGIN_MAX_NESTING_DEPTH: usize = 256;

/// Maximum size of a WAT (text format) module (1 MiB).
///
/// Production plugins are binary `.wasm` files. The text format is accepted
/// for tests and small hand-written plugins; the lower cap keeps text parsing
/// itself from becoming an unbounded pre-scan cost.
#[cfg(feature = "wasm-runtime")]
const PLUGIN_MAX_WAT_BYTES: usize = 1024 * 1024;

/// Name of the out-of-process compiler binary.
///
/// The parent looks for it next to its own executable (for example
/// `target/debug/coderex-wasm-compile`).
#[cfg(feature = "wasm-runtime")]
const WASM_COMPILE_HELPER: &str = "coderex-wasm-compile";

/// CPU-seconds limit installed on the compiler child (`RLIMIT_CPU` on Unix,
/// job user time on Windows).
#[cfg(feature = "wasm-runtime")]
const WASM_COMPILE_CPU_SECONDS: u64 = 20;

/// Address-space limit installed on the compiler child (`RLIMIT_AS` on Unix,
/// job memory limit on Windows): 2 GiB.
#[cfg(feature = "wasm-runtime")]
const WASM_COMPILE_MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Wall-clock limit for the compiler child. A child that exceeds it is killed.
#[cfg(feature = "wasm-runtime")]
const WASM_COMPILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Environment override for the compiler helper path.
///
/// Compiled into debug builds only; it exists so the subprocess tests can
/// exercise both the helper and the missing-helper fallback.
#[cfg(all(feature = "wasm-runtime", debug_assertions))]
const WASM_COMPILE_HELPER_ENV: &str = "CODEREX_WASM_COMPILE_HELPER";

/// Environment override for the wall-clock timeout, in milliseconds.
///
/// Compiled into debug builds only; used by the "slow helper is killed" test.
#[cfg(all(feature = "wasm-runtime", debug_assertions))]
const WASM_COMPILE_TIMEOUT_ENV: &str = "CODEREX_WASM_COMPILE_TIMEOUT_MS";

/// Build the sandboxed engine shared by every plugin instance.
///
/// Plugins are untrusted code, so the engine is configured with:
/// - fuel metering, so runaway execution is interrupted instead of hanging;
/// - the threads proposal disabled, so a module cannot create a shared memory
///   (which wasmtime's store limiter deliberately does not bound) or use
///   atomics.
///
/// No WASI (or any other host interface) is linked: instances are created with
/// an empty import set, so a module that imports anything at all fails to
/// instantiate. The plugin ABI is self-contained and needs no imports.
///
/// Compilation itself is not bounded here: wasmtime 20 has no compile-time
/// fuel or epoch setting. The only enforced compile-time control is the input
/// size cap in [`PluginWasmInstance::new`]. `parallel_compilation(false)` was
/// considered and deliberately not set: it chooses single-threaded compilation
/// rather than reducing the total work, so it is not a compile-time bound.
#[cfg(feature = "wasm-runtime")]
fn sandboxed_engine() -> Result<wasmtime::Engine, String> {
    let mut config = wasmtime::Config::new();
    config.consume_fuel(true);
    config.wasm_threads(false);
    wasmtime::Engine::new(&config).map_err(|e| format!("failed to build WASM engine: {e}"))
}

/// Reject a module whose length exceeds [`PLUGIN_MAX_WASM_BYTES`].
///
/// Kept separate from [`PluginWasmInstance::new`] so both the pre-read
/// metadata check and the post-read length check report the same error.
#[cfg(feature = "wasm-runtime")]
fn ensure_within_size_cap(wasm_path: &std::path::Path, len: u64) -> Result<(), String> {
    if len > PLUGIN_MAX_WASM_BYTES as u64 {
        return Err(format!(
            "WASM module '{}' is {len} bytes, which exceeds the {} byte plugin limit \
             (PLUGIN_MAX_WASM_BYTES)",
            wasm_path.display(),
            PLUGIN_MAX_WASM_BYTES
        ));
    }
    Ok(())
}

/// Pre-scan limits. Kept as a struct so tests can exercise each limit with a
/// tiny module instead of building an 8 MiB pathological one.
#[cfg(feature = "wasm-runtime")]
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_field_names)]
struct PrescanLimits {
    max_code_section_bytes: usize,
    max_functions: usize,
    max_operators: usize,
    max_nesting_depth: usize,
}

#[cfg(feature = "wasm-runtime")]
impl Default for PrescanLimits {
    fn default() -> Self {
        Self {
            max_code_section_bytes: PLUGIN_MAX_CODE_SECTION_BYTES,
            max_functions: PLUGIN_MAX_FUNCTIONS,
            max_operators: PLUGIN_MAX_OPERATORS,
            max_nesting_depth: PLUGIN_MAX_NESTING_DEPTH,
        }
    }
}

/// Linearly scan `bytes` and reject a module that asks the compiler for too
/// much work, before any compiler sees it.
///
/// The scan enforces four independent limits:
/// - the code section's byte length;
/// - the number of declared functions;
/// - the total number of operators across every function body;
/// - the deepest `block`/`loop`/`if` nesting.
///
/// It also translates WAT input to the binary format first, so the text form
/// cannot bypass the limits that the binary form is subject to. On success it
/// returns the exact bytes that were scanned (an owned copy for WAT input), so
/// the caller compiles the same bytes the scan approved.
///
/// This is a work bound, not a correctness check: it deliberately does not
/// run wasmparser's validator. wasmtime still validates the module.
#[cfg(feature = "wasm-runtime")]
fn prescan_module<'a>(
    name: &str,
    bytes: &'a [u8],
    limits: PrescanLimits,
) -> Result<std::borrow::Cow<'a, [u8]>, String> {
    let binary = if bytes.starts_with(b"\0asm") {
        std::borrow::Cow::Borrowed(bytes)
    } else {
        if bytes.len() > PLUGIN_MAX_WAT_BYTES {
            return Err(format!(
                "WASM module '{name}' is not a binary module and its {} byte text form \
                 exceeds the {PLUGIN_MAX_WAT_BYTES} byte plugin limit (PLUGIN_MAX_WAT_BYTES)",
                bytes.len()
            ));
        }
        wat::parse_bytes(bytes)
            .map_err(|e| format!("failed to parse WASM text module '{name}': {e}"))?
    };

    let mut function_count = 0usize;
    let mut operator_count = 0usize;
    let mut code_section_bytes = 0usize;

    for payload in wasmparser::Parser::new(0).parse_all(&binary) {
        let payload = payload.map_err(|e| format!("failed to pre-scan WASM module '{name}': {e}"))?;
        match payload {
            wasmparser::Payload::FunctionSection(reader) => {
                function_count = function_count.max(reader.count() as usize);
                if function_count > limits.max_functions {
                    return Err(format!(
                        "WASM module '{name}' declares {function_count} functions, which exceeds \
                         the {} function plugin limit (PLUGIN_MAX_FUNCTIONS)",
                        limits.max_functions
                    ));
                }
            }
            wasmparser::Payload::CodeSectionStart { count, range, .. } => {
                // The function section normally precedes the code section; the
                // code section's own count is a second, independent view.
                function_count = function_count.max(count as usize);
                if function_count > limits.max_functions {
                    return Err(format!(
                        "WASM module '{name}' declares {function_count} functions, which exceeds \
                         the {} function plugin limit (PLUGIN_MAX_FUNCTIONS)",
                        limits.max_functions
                    ));
                }
                code_section_bytes = code_section_bytes.max(range.end.saturating_sub(range.start));
                if code_section_bytes > limits.max_code_section_bytes {
                    return Err(format!(
                        "WASM module '{name}' has a {code_section_bytes} byte code section, which \
                         exceeds the {} byte plugin limit (PLUGIN_MAX_CODE_SECTION_BYTES)",
                        limits.max_code_section_bytes
                    ));
                }
            }
            wasmparser::Payload::CodeSectionEntry(body) => {
                let mut reader = body
                    .get_operators_reader()
                    .map_err(|e| format!("failed to pre-scan WASM module '{name}': {e}"))?;
                let mut depth = 0usize;
                while !reader.eof() {
                    let operator = reader
                        .read()
                        .map_err(|e| format!("failed to pre-scan WASM module '{name}': {e}"))?;
                    operator_count += 1;
                    if operator_count > limits.max_operators {
                        return Err(format!(
                            "WASM module '{name}' contains more than {} operators, which exceeds \
                             the operator plugin limit (PLUGIN_MAX_OPERATORS)",
                            limits.max_operators
                        ));
                    }
                    match operator {
                        wasmparser::Operator::Block { .. }
                        | wasmparser::Operator::Loop { .. }
                        | wasmparser::Operator::If { .. } => {
                            depth += 1;
                            if depth > limits.max_nesting_depth {
                                return Err(format!(
                                    "WASM module '{name}' nests block/loop/if {depth} levels deep, \
                                     which exceeds the {} level plugin limit \
                                     (PLUGIN_MAX_NESTING_DEPTH)",
                                    limits.max_nesting_depth
                                ));
                            }
                        }
                        // `else` stays in the same frame; `end` closes one.
                        wasmparser::Operator::End => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(binary)
}

/// Find the out-of-process compiler next to the current executable.
///
/// In debug builds `CODEREX_WASM_COMPILE_HELPER` overrides the path, which the
/// tests use to point at the real helper, a deliberately slow one, or a
/// nonexistent path (the fallback case).
#[cfg(feature = "wasm-runtime")]
fn compile_helper_path() -> Option<std::path::PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(overridden) = std::env::var_os(WASM_COMPILE_HELPER_ENV) {
        let path = std::path::PathBuf::from(overridden);
        return path.is_file().then_some(path);
    }

    let current = std::env::current_exe().ok()?;
    let candidate = current.parent()?.join(format!(
        "{WASM_COMPILE_HELPER}{}",
        std::env::consts::EXE_SUFFIX
    ));
    candidate.is_file().then_some(candidate)
}

/// Wall-clock timeout for the compiler child.
#[cfg(feature = "wasm-runtime")]
fn compile_timeout() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var(WASM_COMPILE_TIMEOUT_ENV) {
        if let Ok(millis) = raw.parse::<u64>() {
            if millis > 0 {
                return std::time::Duration::from_millis(millis);
            }
        }
    }
    WASM_COMPILE_TIMEOUT
}

/// Compile a module, preferring the out-of-process helper.
///
/// The pre-scan has already run on `binary` by the time this is called. When
/// the helper exists, compilation happens in a child process that the parent
/// can kill. When it does not, compilation falls back in-process under the
/// pre-scan limits; that fallback is *not* CPU-bounded, which is why the
/// fallback is logged loudly.
#[cfg(feature = "wasm-runtime")]
fn compile_module(
    engine: &wasmtime::Engine,
    wasm_path: &std::path::Path,
    binary: &[u8],
) -> Result<wasmtime::Module, String> {
    let Some(helper) = compile_helper_path() else {
        tracing::warn!(
            module = %wasm_path.display(),
            "WASM compile helper '{WASM_COMPILE_HELPER}' was not found next to the current \
             executable; compiling in-process. Pre-scan limits apply, but in-process compilation \
             has no CPU bound."
        );
        return wasmtime::Module::new(engine, binary)
            .map_err(|e| format!("failed to load WASM module '{}': {e}", wasm_path.display()));
    };

    let serialized = run_compile_helper(&helper, binary)?;

    // SAFETY: `serialized` is the `Module::serialize` output of our own helper
    // binary, linked against the same wasmtime version and built with the same
    // engine configuration (`sandboxed_engine`). The helper writes it into a
    // private `tempfile` directory (mode 0700 on Unix) and the parent reads it
    // only after the helper exits successfully. wasmtime's safety requirement
    // is that the bytes are trusted precompiled code; the helper compiles an
    // untrusted module, but the bytes it emits are produced by wasmtime itself.
    unsafe { wasmtime::Module::deserialize(engine, &serialized) }.map_err(|e| {
        format!(
            "failed to deserialize compiled WASM module '{}': {e}",
            wasm_path.display()
        )
    })
}

/// Run the helper in a killable child and return its serialized module.
#[cfg(feature = "wasm-runtime")]
fn run_compile_helper(
    helper: &std::path::Path,
    binary: &[u8],
) -> Result<Vec<u8>, String> {
    let dir = tempfile::tempdir()
        .map_err(|e| format!("failed to create a temp dir for WASM compilation: {e}"))?;
    let input_path = dir.path().join("input.wasm");
    let output_path = dir.path().join("output.cwasm");
    let stderr_path = dir.path().join("helper.stderr");

    std::fs::write(&input_path, binary)
        .map_err(|e| format!("failed to stage WASM input for compilation: {e}"))?;
    let stderr = std::fs::File::create(&stderr_path)
        .map_err(|e| format!("failed to create WASM compiler stderr file: {e}"))?;

    let mut command = std::process::Command::new(helper);
    command
        .arg(&input_path)
        .arg(&output_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `pre_exec` runs between fork and exec in the child. The
        // closure only calls `setrlimit`, which is async-signal-safe, and
        // captures two integers.
        unsafe {
            command.pre_exec(|| {
                apply_compile_rlimits()?;
                Ok(())
            });
        }
    }

    #[cfg(target_os = "windows")]
    let sandbox = {
        use coderex_core::sandbox::{SandboxConfig, SandboxHandle};
        let config = SandboxConfig {
            max_cpu_seconds: Some(WASM_COMPILE_CPU_SECONDS),
            max_memory_bytes: Some(WASM_COMPILE_MEMORY_BYTES),
            ..SandboxConfig::default()
        };
        let handle = SandboxHandle::create(config)
            .map_err(|e| format!("failed to create WASM compile job object: {e}"))?;
        handle
            .prepare(&mut command)
            .map_err(|e| format!("failed to prepare WASM compile helper: {e}"))?;
        handle
    };

    let mut child = command.spawn().map_err(|e| {
        format!(
            "failed to spawn WASM compile helper '{}': {e}",
            helper.display()
        )
    })?;

    #[cfg(target_os = "windows")]
    sandbox.post_spawn(&child).map_err(|e| {
        let _ = child.kill();
        format!("failed to assign WASM compile helper to its job object: {e}")
    })?;

    let status = wait_for_compile_child(&mut child, compile_timeout())?;
    if !status.success() {
        let detail = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            format!("WASM compile helper '{}' failed with {status}", helper.display())
        } else {
            format!(
                "WASM compile helper '{}' failed with {status}: {detail}",
                helper.display()
            )
        });
    }

    let serialized = std::fs::read(&output_path).map_err(|e| {
        format!(
            "WASM compile helper '{}' exited successfully but produced no output: {e}",
            helper.display()
        )
    })?;
    if serialized.is_empty() {
        return Err(format!(
            "WASM compile helper '{}' produced an empty serialized module",
            helper.display()
        ));
    }
    Ok(serialized)
}

/// Install `RLIMIT_CPU` and `RLIMIT_AS` on the compiler child.
///
/// `RLIMIT_CPU` is mandatory: without it this process has no hard CPU bound.
/// `RLIMIT_AS` is best-effort because some kernels reject finite address-space
/// limits — Darwin does, returning `EINVAL` for both the soft and hard
/// fields, even for the process's own child. On those platforms the child is
/// still bounded by CPU time and by the parent's wall-clock kill; there is no
/// memory ceiling. The failure cannot be logged here (`pre_exec` is not
/// async-signal-safe), so the documentation carries the caveat.
#[cfg(all(unix, feature = "wasm-runtime"))]
fn apply_compile_rlimits() -> std::io::Result<()> {
    use libc::{rlimit, setrlimit, RLIMIT_AS, RLIMIT_CPU};

    let cpu = rlimit {
        rlim_cur: WASM_COMPILE_CPU_SECONDS,
        rlim_max: WASM_COMPILE_CPU_SECONDS,
    };
    // SAFETY: `setrlimit` is async-signal-safe and this runs in the forked
    // child before exec.
    if unsafe { setrlimit(RLIMIT_CPU, std::ptr::from_ref(&cpu)) } != 0 {
        let error = std::io::Error::last_os_error();
        return Err(std::io::Error::new(
            error.kind(),
            format!("setrlimit(RLIMIT_CPU) failed: {error}"),
        ));
    }

    let memory = rlimit {
        rlim_cur: WASM_COMPILE_MEMORY_BYTES,
        rlim_max: WASM_COMPILE_MEMORY_BYTES,
    };
    // SAFETY: as above. Failure is deliberately ignored: see the doc comment.
    let _ = unsafe { setrlimit(RLIMIT_AS, std::ptr::from_ref(&memory)) };

    Ok(())
}

/// Wait for the compiler child, killing it when the wall-clock budget expires.
#[cfg(feature = "wasm-runtime")]
fn wait_for_compile_child(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> Result<std::process::ExitStatus, String> {
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "WASM compilation exceeded the {} ms wall-clock limit; the helper was \
                         killed",
                        timeout.as_millis()
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("failed to wait for the WASM compile helper: {e}"));
            }
        }
    }
}

/// Compile a WASM module and return wasmtime's serialized representation.
///
/// Internal entry point for the hidden `coderex-wasm-compile` helper binary;
/// not part of the stable plugin API. The pre-scan runs here too so a directly
/// invoked helper still refuses pathological input.
#[cfg(feature = "wasm-runtime")]
#[doc(hidden)]
pub fn compile_to_serialized(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let scanned = prescan_module("<helper input>", bytes, PrescanLimits::default())?;
    let engine = sandboxed_engine()?;
    let module = wasmtime::Module::new(&engine, &*scanned)
        .map_err(|e| format!("failed to compile WASM module: {e}"))?;
    module
        .serialize()
        .map_err(|e| format!("failed to serialize compiled WASM module: {e}"))
}

/// Deserialize bytes produced by [`compile_to_serialized`].
///
/// Internal entry point used by the subprocess round-trip test; not part of
/// the stable plugin API.
#[cfg(feature = "wasm-runtime")]
#[doc(hidden)]
pub fn deserialize_compiled_module(bytes: &[u8]) -> Result<(), String> {
    let engine = sandboxed_engine()?;
    // SAFETY: see the safety comment in `compile_module`. This entry point is
    // an internal test hook; it trusts the caller the same way the parent
    // trusts its own helper's output.
    unsafe { wasmtime::Module::deserialize(&engine, bytes) }
        .map(|_| ())
        .map_err(|e| format!("failed to deserialize compiled WASM module: {e}"))
}

/// A loaded WASM plugin instance.
///
/// When the `wasm-runtime` feature is enabled, this compiles and instantiates
/// a `.wasm` module on demand for each call.  The compiled `Engine` and
/// `Module` are cached; a fresh `Store` + `Instance` are created per call so
/// the type remains `Send` and can live in `DashMap`.
pub struct PluginWasmInstance {
    #[cfg(feature = "wasm-runtime")]
    engine: wasmtime::Engine,
    #[cfg(feature = "wasm-runtime")]
    module: wasmtime::Module,
    #[cfg(feature = "wasm-runtime")]
    initialized: bool,
    #[cfg(feature = "wasm-runtime")]
    commands: Vec<PluginCommandInfo>,
    #[cfg(feature = "wasm-runtime")]
    skills: Vec<PluginSkillInfo>,
    #[cfg(not(feature = "wasm-runtime"))]
    _marker: std::marker::PhantomData<()>,
}

impl std::fmt::Debug for PluginWasmInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(feature = "wasm-runtime")]
        {
            f.debug_struct("PluginWasmInstance")
                .field("initialized", &self.initialized)
                .field("commands", &self.commands)
                .field("skills", &self.skills)
                .finish_non_exhaustive()
        }
        #[cfg(not(feature = "wasm-runtime"))]
        {
            f.debug_struct("PluginWasmInstance")
                .field("enabled", &false)
                .finish()
        }
    }
}

#[cfg(feature = "wasm-runtime")]
impl PluginWasmInstance {
    /// Load a WASM module from the given path.
    ///
    /// The file is rejected before compilation if it is larger than
    /// `PLUGIN_MAX_WASM_BYTES`. The metadata check avoids reading an oversized
    /// file into memory at all; the length is checked again after reading in
    /// case the file changed between the two calls.
    pub fn new(wasm_path: &std::path::Path) -> Result<Self, String> {
        let engine = sandboxed_engine()?;

        let metadata = std::fs::metadata(wasm_path)
            .map_err(|e| format!("failed to read WASM module '{}': {e}", wasm_path.display()))?;
        ensure_within_size_cap(wasm_path, metadata.len())?;
        let bytes = std::fs::read(wasm_path)
            .map_err(|e| format!("failed to read WASM module '{}': {e}", wasm_path.display()))?;
        ensure_within_size_cap(wasm_path, bytes.len() as u64)?;

        // The linear pre-scan is the floor: it runs before either compilation
        // path, so even the in-process fallback refuses modules that demand
        // excessive code size, function count, operator count, or nesting.
        let name = wasm_path.display().to_string();
        let scanned = prescan_module(&name, &bytes, PrescanLimits::default())?;

        let module = compile_module(&engine, wasm_path, &scanned)?;
        Ok(Self {
            engine,
            module,
            initialized: false,
            commands: Vec::new(),
            skills: Vec::new(),
        })
    }

    /// Create a fresh, resource-limited `Store` for a single plugin call.
    ///
    /// Every call gets its own store, so the fuel budget and memory ceiling are
    /// reset per call rather than accumulating across calls.
    fn new_store(&self) -> Result<wasmtime::Store<wasmtime::StoreLimits>, String> {
        let limits = wasmtime::StoreLimitsBuilder::new()
            .memory_size(PLUGIN_MAX_MEMORY_BYTES)
            .build();
        let mut store = wasmtime::Store::new(&self.engine, limits);
        store.limiter(|limits| limits);
        store
            .set_fuel(PLUGIN_FUEL)
            .map_err(|e| format!("failed to set WASM fuel: {e}"))?;
        Ok(store)
    }

    /// Check if initialized.
    #[must_use]
    pub const fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Get discovered commands.
    #[must_use]
    pub fn commands(&self) -> &[PluginCommandInfo] {
        &self.commands
    }

    /// Get discovered skills.
    #[must_use]
    pub fn skills(&self) -> &[PluginSkillInfo] {
        &self.skills
    }

    /// Perform the coderex initialize handshake.
    pub async fn initialize(&mut self) -> Result<serde_json::Value, String> {
        let config = serde_json::json!({
            "protocolVersion": "1.0",
            "capabilities": {},
            "clientInfo": {
                "name": "coderex",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        let result = self
            .call_with_json("coderex_initialize", Some(&config.to_string()))
            .await?;
        self.initialized = true;
        Ok(result)
    }

    /// Discover commands from the plugin.
    pub async fn list_commands(&mut self) -> Result<Vec<PluginCommandInfo>, String> {
        let result = self.call_with_json("coderex_commands_list", None).await?;
        let commands: Vec<PluginCommandInfo> = serde_json::from_value(result)
            .map_err(|e| format!("failed to parse commands/list response: {e}"))?;
        self.commands = commands.clone();
        Ok(commands)
    }

    /// Discover skills from the plugin.
    pub async fn list_skills(&mut self) -> Result<Vec<PluginSkillInfo>, String> {
        let result = self.call_with_json("coderex_skills_list", None).await?;
        let skills: Vec<PluginSkillInfo> = serde_json::from_value(result)
            .map_err(|e| format!("failed to parse skills/list response: {e}"))?;
        self.skills = skills.clone();
        Ok(skills)
    }

    /// Execute a plugin command.
    pub async fn execute_command(&mut self, name: &str, args: &[String]) -> Result<String, String> {
        let args_json =
            serde_json::to_string(args).map_err(|e| format!("failed to serialize args: {e}"))?;
        let result = self
            .call_with_two_strings("coderex_command_execute", name, &args_json)
            .await?;
        match result {
            serde_json::Value::String(s) => Ok(s),
            other => Ok(other.to_string()),
        }
    }

    /// Execute a plugin skill.
    pub async fn execute_skill(
        &mut self,
        name: &str,
        context: &serde_json::Value,
    ) -> Result<String, String> {
        let ctx_json = context.to_string();
        let result = self
            .call_with_two_strings("coderex_skill_execute", name, &ctx_json)
            .await?;
        match result {
            serde_json::Value::String(s) => Ok(s),
            other => Ok(other.to_string()),
        }
    }

    /// Internal helper: call a guest function that takes an optional JSON
    /// string and returns a JSON string pointer.
    async fn call_with_json(
        &self,
        method: &str,
        params_json: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        tokio::task::block_in_place(|| {
            let mut store = self.new_store()?;
            let instance = wasmtime::Instance::new(&mut store, &self.module, &[])
                .map_err(|e| format!("failed to instantiate WASM module: {e}"))?;

            let memory = instance
                .get_memory(&mut store, "memory")
                .ok_or("WASM module does not export 'memory'")?;
            let alloc = instance
                .get_typed_func::<i32, i32>(&mut store, "alloc")
                .map_err(|e| format!("WASM module missing 'alloc': {e}"))?;
            let dealloc = instance
                .get_typed_func::<(i32, i32), ()>(&mut store, "dealloc")
                .map_err(|e| format!("WASM module missing 'dealloc': {e}"))?;

            let result_ptr = if let Some(json) = params_json {
                let func = instance
                    .get_typed_func::<(i32, i32), i32>(&mut store, method)
                    .map_err(|e| format!("WASM module missing '{method}': {e}"))?;
                let (ptr, len) = write_to_memory(&memory, &mut store, &alloc, json)?;
                let r = func
                    .call(&mut store, (ptr, len))
                    .map_err(|e| format!("WASM call '{method}' failed: {e:#}"))?;
                let _ = dealloc.call(&mut store, (ptr, len));
                r
            } else {
                let func = instance
                    .get_typed_func::<(), i32>(&mut store, method)
                    .map_err(|e| format!("WASM module missing '{method}': {e}"))?;
                func.call(&mut store, ())
                    .map_err(|e| format!("WASM call '{method}' failed: {e}"))?
            };

            let result_json = read_cstring(&memory, &store, result_ptr)?;
            let _ = dealloc.call(&mut store, (result_ptr, result_json.len() as i32));

            serde_json::from_str(&result_json)
                .map_err(|e| format!("failed to parse WASM JSON result: {e}"))
        })
    }

    /// Internal helper: call a guest function that takes two strings
    /// `(ptr1, len1, ptr2, len2) -> i32`.
    async fn call_with_two_strings(
        &self,
        method: &str,
        s1: &str,
        s2: &str,
    ) -> Result<serde_json::Value, String> {
        tokio::task::block_in_place(|| {
            let mut store = self.new_store()?;
            let instance = wasmtime::Instance::new(&mut store, &self.module, &[])
                .map_err(|e| format!("failed to instantiate WASM module: {e}"))?;

            let memory = instance
                .get_memory(&mut store, "memory")
                .ok_or("WASM module does not export 'memory'")?;
            let alloc = instance
                .get_typed_func::<i32, i32>(&mut store, "alloc")
                .map_err(|e| format!("WASM module missing 'alloc': {e}"))?;
            let dealloc = instance
                .get_typed_func::<(i32, i32), ()>(&mut store, "dealloc")
                .map_err(|e| format!("WASM module missing 'dealloc': {e}"))?;
            let func = instance
                .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, method)
                .map_err(|e| format!("WASM module missing '{method}': {e}"))?;

            let (p1, l1) = write_to_memory(&memory, &mut store, &alloc, s1)?;
            let (p2, l2) = write_to_memory(&memory, &mut store, &alloc, s2)?;

            let result_ptr = func
                .call(&mut store, (p1, l1, p2, l2))
                .map_err(|e| format!("WASM call '{method}' failed: {e:#}"))?;

            let _ = dealloc.call(&mut store, (p1, l1));
            let _ = dealloc.call(&mut store, (p2, l2));

            let result_json = read_cstring(&memory, &store, result_ptr)?;
            let _ = dealloc.call(&mut store, (result_ptr, result_json.len() as i32));

            serde_json::from_str(&result_json)
                .map_err(|e| format!("failed to parse WASM JSON result: {e}"))
        })
    }
}

#[cfg(not(feature = "wasm-runtime"))]
impl PluginWasmInstance {
    /// Stub constructor when the feature is disabled.
    pub fn new(_wasm_path: &std::path::Path) -> Result<Self, String> {
        Err("WASM runtime not enabled. Rebuild with --features wasm-runtime".into())
    }

    /// Always `false` when the feature is disabled.
    #[must_use]
    pub const fn is_initialized(&self) -> bool {
        false
    }

    /// Always empty when the feature is disabled.
    #[must_use]
    pub fn commands(&self) -> &[PluginCommandInfo] {
        &[]
    }

    /// Always empty when the feature is disabled.
    #[must_use]
    pub fn skills(&self) -> &[PluginSkillInfo] {
        &[]
    }

    /// Returns an error when the feature is disabled.
    pub async fn initialize(&mut self) -> Result<serde_json::Value, String> {
        Err("WASM runtime not enabled".into())
    }

    /// Returns an error when the feature is disabled.
    pub async fn list_commands(&mut self) -> Result<Vec<PluginCommandInfo>, String> {
        Err("WASM runtime not enabled".into())
    }

    /// Returns an error when the feature is disabled.
    pub async fn list_skills(&mut self) -> Result<Vec<PluginSkillInfo>, String> {
        Err("WASM runtime not enabled".into())
    }

    /// Returns an error when the feature is disabled.
    pub async fn execute_command(
        &mut self,
        _name: &str,
        _args: &[String],
    ) -> Result<String, String> {
        Err("WASM runtime not enabled".into())
    }

    /// Returns an error when the feature is disabled.
    pub async fn execute_skill(
        &mut self,
        _name: &str,
        _context: &serde_json::Value,
    ) -> Result<String, String> {
        Err("WASM runtime not enabled".into())
    }
}

#[cfg(feature = "wasm-runtime")]
fn write_to_memory(
    memory: &wasmtime::Memory,
    store: &mut wasmtime::Store<wasmtime::StoreLimits>,
    alloc: &wasmtime::TypedFunc<i32, i32>,
    data: &str,
) -> Result<(i32, i32), String> {
    let bytes = data.as_bytes();
    let ptr = alloc
        .call(&mut *store, bytes.len() as i32)
        .map_err(|e| format!("alloc failed: {e}"))?;
    let mem_data = memory.data_mut(store);
    let offset = ptr as usize;
    if offset + bytes.len() > mem_data.len() {
        return Err("WASM alloc returned pointer outside memory bounds".into());
    }
    mem_data[offset..offset + bytes.len()].copy_from_slice(bytes);
    Ok((ptr, bytes.len() as i32))
}

#[cfg(feature = "wasm-runtime")]
fn read_cstring(
    memory: &wasmtime::Memory,
    store: &wasmtime::Store<wasmtime::StoreLimits>,
    ptr: i32,
) -> Result<String, String> {
    if ptr == 0 {
        return Err("WASM returned null pointer".into());
    }
    let offset = ptr as usize;
    let mem_data = memory.data(store);
    if offset >= mem_data.len() {
        return Err("WASM pointer out of bounds".into());
    }
    let mut end = offset;
    while end < mem_data.len() && mem_data[end] != 0 {
        end += 1;
        if end - offset > 1024 * 1024 {
            return Err("WASM string exceeds 1MB limit".into());
        }
    }
    let bytes = &mem_data[offset..end];
    String::from_utf8(bytes.to_vec()).map_err(|e| format!("WASM string is not valid UTF-8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_instance_not_initialized() {
        // Without wasm-runtime every path fails; with it, a missing file does.
        let result = PluginWasmInstance::new(std::path::Path::new("/nonexistent/plugin.wasm"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("not enabled")
                || err.contains("failed to read")
                || err.contains("failed to load"),
            "unexpected error: {err}"
        );
    }
}

/// Sandbox tests. These only exist when the WASM runtime is compiled in, which
/// it is by default: `wasm-runtime` is part of the crate's default features.
///
/// Each test writes a small WAT module to a temp file and asserts the sandbox
/// stops behaviour a hostile plugin would use to take the host down.
#[cfg(all(test, feature = "wasm-runtime"))]
mod sandbox_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Write a WAT module to a temp dir and return its path.
    fn write_module(dir: &Path, name: &str, wat: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, wat).expect("failed to write WAT module");
        path
    }

    /// A file over the cap is refused by the size check, before wasmtime
    /// parses it. The contents are zeros, so a missing check would surface as
    /// a parse error instead of this size error.
    #[test]
    fn module_over_size_cap_is_rejected_before_compilation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.wasm");
        let size = PLUGIN_MAX_WASM_BYTES + 1;
        std::fs::write(&path, vec![0u8; size]).unwrap();

        let err = PluginWasmInstance::new(&path).unwrap_err();
        assert!(
            err.contains("exceeds") && err.contains(&size.to_string()),
            "expected the size cap to name the oversized module, got: {err}"
        );
    }

    /// A file exactly at the cap passes the file-size gate. It is still not a
    /// valid module, so the pre-scan (or wasmtime's parser) rejects it — but
    /// not the 16 MiB file cap.
    #[test]
    fn module_at_size_cap_reaches_the_prescan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("at-cap.wasm");
        std::fs::write(&path, vec![0u8; PLUGIN_MAX_WASM_BYTES]).unwrap();

        let err = PluginWasmInstance::new(&path).unwrap_err();
        assert!(
            !err.contains("PLUGIN_MAX_WASM_BYTES"),
            "a file at the cap must not be rejected by the file-size gate: {err}"
        );
        assert!(
            err.contains("pre-scan")
                || err.contains("text form")
                || err.contains("failed to load"),
            "expected the pre-scan or wasmtime to reject the file, got: {err}"
        );
    }

    /// A normal module below the cap compiles and loads.
    #[test]
    fn module_below_size_cap_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_module(
            dir.path(),
            "small.wasm",
            r#"(module (memory (export "memory") 1))"#,
        );

        assert!(
            PluginWasmInstance::new(&path).is_ok(),
            "a module below the size cap must load"
        );
    }

    /// A plugin that spins forever must be stopped by the fuel budget rather
    /// than hanging the calling thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runaway_loop_is_stopped_by_fuel() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_module(
            dir.path(),
            "spin.wasm",
            r#"(module
                 (memory (export "memory") 1)
                 (func (export "alloc") (param i32) (result i32)
                   i32.const 1024)
                 (func (export "dealloc") (param i32 i32))
                 (func (export "coderex_initialize") (param i32 i32) (result i32)
                   (loop $spin
                     br $spin)
                   i32.const 0))"#,
        );

        let mut instance = PluginWasmInstance::new(&path).unwrap();
        let err = instance
            .initialize()
            .await
            .expect_err("a module that loops forever must not complete");
        assert!(
            err.contains("fuel"),
            "expected an out-of-fuel trap, got: {err}"
        );
    }

    /// A plugin that grows its memory past the ceiling must be refused. The
    /// module returns a valid pointer only if the growth succeeded, so a
    /// missing limit would surface as a successful call.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn over_limit_memory_growth_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_module(
            dir.path(),
            "grow.wasm",
            r#"(module
                 (memory (export "memory") 1)
                 (data (i32.const 16) "\"ok\"\00")
                 (func (export "alloc") (param i32) (result i32)
                   i32.const 1024)
                 (func (export "dealloc") (param i32 i32))
                 (func (export "coderex_initialize") (param i32 i32) (result i32)
                   ;; ask for 2000 more pages (128 MiB), above the 64 MiB ceiling
                   i32.const 2000
                   memory.grow
                   i32.const 0
                   i32.ge_s
                   (if (result i32)
                     (then i32.const 16)
                     (else i32.const 0))))"#,
        );

        let mut instance = PluginWasmInstance::new(&path).unwrap();
        let err = instance
            .initialize()
            .await
            .expect_err("memory growth past the ceiling must be denied");
        assert!(
            err.contains("null pointer"),
            "expected the growth to be denied, got: {err}"
        );
    }

    /// A module whose *declared* memory already exceeds the ceiling must fail
    /// to instantiate at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn over_limit_initial_memory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_module(
            dir.path(),
            "big.wasm",
            r#"(module
                 (memory (export "memory") 2000)
                 (data (i32.const 16) "\"ok\"\00")
                 (func (export "alloc") (param i32) (result i32)
                   i32.const 1024)
                 (func (export "dealloc") (param i32 i32))
                 (func (export "coderex_initialize") (param i32 i32) (result i32)
                   i32.const 16))"#,
        );

        let mut instance = PluginWasmInstance::new(&path).unwrap();
        let err = instance
            .initialize()
            .await
            .expect_err("a module declaring 128 MiB must not instantiate");
        assert!(
            err.contains("memory"),
            "expected an instantiation memory-limit error, got: {err}"
        );
    }

    /// The threads proposal is off, so a module using a shared memory (which
    /// wasmtime's store limiter does not bound) is rejected at load time.
    #[test]
    fn shared_memory_module_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_module(
            dir.path(),
            "shared.wasm",
            r#"(module
                 (memory (export "memory") 1 1 shared)
                 (func (export "alloc") (param i32) (result i32)
                   i32.const 0))"#,
        );

        assert!(
            PluginWasmInstance::new(&path).is_err(),
            "a module using shared memory must not load"
        );
    }

    /// No host interface is linked, so a module importing WASI cannot be
    /// instantiated. This is a regression guard for the "no WASI" property,
    /// not a fix: it holds regardless of the limits above.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wasi_importing_module_cannot_instantiate() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_module(
            dir.path(),
            "wasi.wasm",
            r#"(module
                 (import "wasi_snapshot_preview1" "fd_write"
                   (func (param i32 i32 i32 i32) (result i32)))
                 (memory (export "memory") 1)
                 (func (export "alloc") (param i32) (result i32)
                   i32.const 1024)
                 (func (export "dealloc") (param i32 i32))
                 (func (export "coderex_initialize") (param i32 i32) (result i32)
                   i32.const 0))"#,
        );

        let mut instance = PluginWasmInstance::new(&path).unwrap();
        let err = instance
            .initialize()
            .await
            .expect_err("a module importing WASI must not instantiate");
        assert!(
            err.contains("instantiate"),
            "expected an unresolved-import error, got: {err}"
        );
    }
}

/// Pre-scan tests. Each limit is exercised with a deliberately tiny cap so the
/// test module stays small; the production caps are constants in this file.
#[cfg(all(test, feature = "wasm-runtime"))]
mod prescan_tests {
    use super::*;

    /// A normal module is accepted and translated to binary.
    #[test]
    fn normal_module_passes_prescan() {
        let wat = br#"(module
                        (memory (export "memory") 1)
                        (func (export "f") nop))"#;
        let scanned =
            prescan_module("normal.wat", wat, PrescanLimits::default()).expect("normal module");
        assert!(
            scanned.starts_with(b"\0asm"),
            "the scan must return binary WASM for WAT input"
        );
    }

    /// A code section larger than the cap is rejected before compilation.
    #[test]
    fn oversized_code_section_is_rejected() {
        let limits = PrescanLimits {
            max_code_section_bytes: 8,
            ..PrescanLimits::default()
        };
        let wat = br"(module (func nop nop nop nop nop nop nop nop nop nop nop nop nop nop nop nop))";

        let err = prescan_module("wide.wat", wat, limits).unwrap_err();
        assert!(
            err.contains("code section") && err.contains("PLUGIN_MAX_CODE_SECTION_BYTES"),
            "expected the code-section cap to name itself, got: {err}"
        );
        assert!(
            err.contains("8 byte plugin limit"),
            "expected the error to name the configured cap, got: {err}"
        );
    }

    /// Too many declared functions is rejected.
    #[test]
    fn too_many_functions_is_rejected() {
        let limits = PrescanLimits {
            max_functions: 1,
            ..PrescanLimits::default()
        };
        let wat = br"(module (func) (func))";

        let err = prescan_module("many.wat", wat, limits).unwrap_err();
        assert!(
            err.contains("functions") && err.contains("PLUGIN_MAX_FUNCTIONS"),
            "expected the function-count cap to name itself, got: {err}"
        );
    }

    /// Too many operators across function bodies is rejected.
    #[test]
    fn too_many_operators_is_rejected() {
        let limits = PrescanLimits {
            max_operators: 2,
            ..PrescanLimits::default()
        };
        let wat = br"(module (func nop nop nop))";

        let err = prescan_module("busy.wat", wat, limits).unwrap_err();
        assert!(
            err.contains("operators") && err.contains("PLUGIN_MAX_OPERATORS"),
            "expected the operator cap to name itself, got: {err}"
        );
    }

    /// Excessive block/loop/if nesting is rejected.
    #[test]
    fn excessive_nesting_is_rejected() {
        let limits = PrescanLimits {
            max_nesting_depth: 2,
            ..PrescanLimits::default()
        };
        let wat = br"(module (func block block block nop end end end))";

        let err = prescan_module("deep.wat", wat, limits).unwrap_err();
        assert!(
            err.contains("nests block/loop/if") && err.contains("PLUGIN_MAX_NESTING_DEPTH"),
            "expected the nesting cap to name itself, got: {err}"
        );
        assert!(
            err.contains("3 levels deep"),
            "expected the error to report the observed depth, got: {err}"
        );
    }

    /// Nesting at exactly the cap is still accepted (the check is `>`).
    #[test]
    fn nesting_at_the_cap_is_accepted() {
        let limits = PrescanLimits {
            max_nesting_depth: 2,
            ..PrescanLimits::default()
        };
        let wat = br"(module (func block block nop end end))";

        prescan_module("at-cap.wat", wat, limits)
            .expect("nesting at the cap must be accepted");
    }
}
