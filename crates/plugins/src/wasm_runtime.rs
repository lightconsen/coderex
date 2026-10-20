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
//!
//! The host also provides one optional import:
//! - `coderex.user_config(ptr: i32, capacity: i32) -> i32` — copy the resolved
//!   `userConfig` options, as a JSON object, into guest memory. It returns the
//!   full JSON length (call with `ptr = 0, capacity = 0` to query it), so a
//!   guest can detect truncation when the return value is greater than the
//!   capacity it passed. See [`PluginWasmInstance::new_with_options`] and
//!   `docs/plugins.md`.
//!
//! A module that imports nothing is still instantiated with an empty import
//! set; linking the one host function does not require any module to use it.

// The compile subprocess needs `pre_exec` to install rlimits between fork and
// exec, and wasmtime 20's `Module::deserialize` is an `unsafe fn` by design
// (it loads precompiled code). Both uses are confined to `#[cfg(unix)]` /
// deserialization sites with the safety arguments spelled out there.
#![allow(unsafe_code)]

use crate::runtime::{PluginCommandInfo, PluginSkillInfo};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, RwLock};

// ─── Host-injected compiler command ─────────────────────────────────────────

/// A compiler command supplied by the host: a program plus leading arguments.
///
/// The runtime appends `<input> <output>` and runs the result as a killable
/// child. The CodeRex CLI configures this to `current_exe()` with
/// [`INTERNAL_WASM_COMPILE_FLAG`], so the installed binary compiles modules
/// itself and no sibling helper is required.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WasmCompilerCommand {
    program: PathBuf,
    args: Vec<OsString>,
}

impl WasmCompilerCommand {
    /// Create a command for `program` with no leading arguments.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// Append one leading argument (for example the hidden subcommand flag).
    #[must_use]
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// The program to execute.
    #[must_use]
    pub fn program(&self) -> &std::path::Path {
        &self.program
    }

    /// Arguments that precede `<input> <output>`.
    #[must_use]
    pub fn args(&self) -> &[OsString] {
        &self.args
    }
}

/// The host-injected compiler command, when one has been configured.
static COMPILER_COMMAND: LazyLock<RwLock<Option<WasmCompilerCommand>>> =
    LazyLock::new(|| RwLock::new(None));

/// An explicit AOT cache directory, when the host configured one.
static CACHE_DIR_OVERRIDE: LazyLock<RwLock<Option<PathBuf>>> = LazyLock::new(|| RwLock::new(None));

/// Inject the compiler command the WASM runtime must use.
///
/// This is the only way production hosts enable WASM compilation: when no
/// command is configured (and no helper is found next to the executable), the
/// runtime refuses to load a WASM plugin rather than compiling untrusted code
/// in-process. The setting is process-wide and should be installed once at
/// startup, before any plugin is loaded.
pub fn set_compiler_command(command: WasmCompilerCommand) {
    let mut slot = COMPILER_COMMAND
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = Some(command);
}

/// The compiler command currently configured by the host, if any.
#[must_use]
pub fn configured_compiler_command() -> Option<WasmCompilerCommand> {
    COMPILER_COMMAND
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Override the AOT cache directory for this process.
///
/// Equivalent to setting `CODEREX_WASM_CACHE_DIR`, but without touching the
/// process environment. The directory is created on the first cache write.
pub fn set_cache_dir(dir: impl Into<PathBuf>) {
    let mut slot = CACHE_DIR_OVERRIDE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = Some(dir.into());
}

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

/// Name of the out-of-process compiler helper binary.
///
/// Kept as a `[[bin]]` target for tests and for hosts that prefer a separate
/// compiler process to the CLI's hidden subcommand. The CodeRex CLI does *not*
/// depend on it: the CLI injects its own executable instead (see
/// [`INTERNAL_WASM_COMPILE_FLAG`]).
#[cfg(feature = "wasm-runtime")]
const WASM_COMPILE_HELPER: &str = "coderex-wasm-compile";

/// Hidden argument that makes the host binary act as the WASM compiler.
///
/// The host runs `current_exe() --internal-wasm-compile <input> <output>`.
/// The CLI handles this before normal argument parsing, so packaged builds and
/// `cargo run` use the same compiler path without shipping a sibling binary.
pub const INTERNAL_WASM_COMPILE_FLAG: &str = "--internal-wasm-compile";

/// CPU-seconds limit installed on the compiler child (`RLIMIT_CPU` on Unix,
/// job user time on Windows).
#[cfg(feature = "wasm-runtime")]
const WASM_COMPILE_CPU_SECONDS: u64 = 20;

/// Memory limit installed on the compiler child (2 GiB).
///
/// On Linux this is an address-space limit (`RLIMIT_AS`); on Darwin it is a
/// data-segment limit (`RLIMIT_DATA`), because Darwin rejects finite
/// address-space limits. On Windows it is the Job Object's job memory limit.
#[cfg(feature = "wasm-runtime")]
const WASM_COMPILE_MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Wall-clock limit for the compiler child. A child that exceeds it is killed.
#[cfg(feature = "wasm-runtime")]
const WASM_COMPILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Environment override for the compiler helper path.
///
/// Compiled into debug builds only; it exists so the subprocess tests can
/// simulate a helper that is missing or that points at a test binary.
#[cfg(all(feature = "wasm-runtime", debug_assertions))]
const WASM_COMPILE_HELPER_ENV: &str = "CODEREX_WASM_COMPILE_HELPER";

/// Environment override for the wall-clock timeout, in milliseconds.
///
/// Compiled into debug builds only; used by the "slow helper is killed" test.
#[cfg(all(feature = "wasm-runtime", debug_assertions))]
const WASM_COMPILE_TIMEOUT_ENV: &str = "CODEREX_WASM_COMPILE_TIMEOUT_MS";

/// Environment override for the AOT cache directory.
///
/// Honoured in every build so operators can place the cache on a different
/// volume (and so tests can isolate it). The platform default is documented in
/// `docs/plugins.md`.
#[cfg(feature = "wasm-runtime")]
const WASM_CACHE_DIR_ENV: &str = "CODEREX_WASM_CACHE_DIR";

/// Total size the AOT cache is pruned back to after a new entry is written.
///
/// The most recently modified entry is kept even when it alone exceeds the
/// bound, so a single very large module cannot make every load a recompile.
#[cfg(feature = "wasm-runtime")]
const WASM_CACHE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Version of the on-disk cache entry envelope.
///
/// A different value invalidates every existing entry (they are deleted when
/// read). It changes only with this file's format, not with wasmtime.
#[cfg(feature = "wasm-runtime")]
const WASM_CACHE_FORMAT_VERSION: u32 = 1;

/// Magic bytes at the start of every cache entry.
#[cfg(feature = "wasm-runtime")]
const WASM_CACHE_MAGIC: &[u8; 8] = b"CRXCWASM";

/// `magic + version + engine hash + module hash + payload length`.
#[cfg(feature = "wasm-runtime")]
const WASM_CACHE_HEADER_LEN: usize = 8 + 4 + 32 + 32 + 8;

/// Build the sandboxed engine shared by every plugin instance.
///
/// Plugins are untrusted code, so the engine is configured with:
/// - fuel metering, so runaway execution is interrupted instead of hanging;
/// - the threads proposal disabled, so a module cannot create a shared memory
///   (which wasmtime's store limiter deliberately does not bound) or use
///   atomics.
///
/// No WASI is linked. The only host interface is the optional
/// `coderex.user_config` import; a module that imports anything else still
/// fails to instantiate, and a module that imports nothing instantiates
/// unchanged. The plugin ABI needs no imports to work.
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
        let payload =
            payload.map_err(|e| format!("failed to pre-scan WASM module '{name}': {e}"))?;
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

/// Find the compiler helper next to the current executable.
///
/// Debug builds also honour `CODEREX_WASM_COMPILE_HELPER`, which the tests use
/// to point at the helper, at a slow one, or at a path that does not exist (to
/// prove the fail-closed behaviour). The search walks up a few directories so
/// unit tests — whose binary lives in `target/<profile>/deps/` — find the
/// helper in `target/<profile>/`.
#[cfg(feature = "wasm-runtime")]
fn helper_next_to_current_exe() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(overridden) = std::env::var_os(WASM_COMPILE_HELPER_ENV) {
        let path = PathBuf::from(overridden);
        // An explicit override is authoritative: when it names a missing
        // file, the load fails closed rather than silently picking another
        // helper.
        return path.is_file().then_some(path);
    }

    let name = format!("{WASM_COMPILE_HELPER}{}", std::env::consts::EXE_SUFFIX);
    let mut dir = std::env::current_exe().ok()?;
    dir.pop();
    for _ in 0..3 {
        let candidate = dir.join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// The compiler command to use when the host has not injected one.
///
/// A host setting wins; otherwise a helper next to the executable is accepted.
/// When neither exists, this returns `None` and loading fails closed.
#[cfg(feature = "wasm-runtime")]
fn resolved_compiler_command() -> Option<WasmCompilerCommand> {
    configured_compiler_command()
        .or_else(|| helper_next_to_current_exe().map(WasmCompilerCommand::new))
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

/// The AOT cache directory, honouring (in order) a host override, the
/// `CODEREX_WASM_CACHE_DIR` environment variable, and the platform default.
#[cfg(feature = "wasm-runtime")]
fn resolved_cache_dir() -> PathBuf {
    let override_dir = CACHE_DIR_OVERRIDE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(dir) = override_dir {
        return dir;
    }
    if let Some(dir) = std::env::var_os(WASM_CACHE_DIR_ENV) {
        return PathBuf::from(dir);
    }
    default_cache_dir()
}

/// The platform's per-user cache directory for compiled WASM modules.
#[cfg(feature = "wasm-runtime")]
fn default_cache_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home::home_dir() {
            return home.join("Library/Caches/coderex/wasm");
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join("coderex/wasm");
            }
        }
        if let Some(home) = home::home_dir() {
            return home.join(".cache/coderex/wasm");
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("coderex").join("wasm");
        }
        if let Some(home) = home::home_dir() {
            return home
                .join("AppData")
                .join("Local")
                .join("coderex")
                .join("wasm");
        }
    }
    std::env::temp_dir().join("coderex-wasm")
}

// ─── AOT cache ──────────────────────────────────────────────────────────────

/// A SHA-256 hasher usable through `std::hash::Hash`.
///
/// wasmtime exposes its compatibility fingerprint only as `impl Hash`; feeding
/// it through SHA-256 gives the cache a fixed-size, process-independent key.
#[cfg(feature = "wasm-runtime")]
#[derive(Clone)]
struct Sha256Hasher(sha2::Sha256);

#[cfg(feature = "wasm-runtime")]
impl std::hash::Hasher for Sha256Hasher {
    fn finish(&self) -> u64 {
        use sha2::Digest;
        let digest = self.0.clone().finalize();
        u64::from_le_bytes(digest[..8].try_into().expect("SHA-256 is 32 bytes"))
    }

    fn write(&mut self, bytes: &[u8]) {
        use sha2::Digest;
        self.0.update(bytes);
    }
}

/// Hash the engine's configuration and wasmtime version.
///
/// `precompile_compatibility_hash` is wasmtime's own answer to "will bytes
/// compiled under this engine deserialize under that one", and it covers the
/// version and every configuration knob that affects code generation. The
/// leading tag is ours: bump it when the cache entry format or the policies in
/// [`sandboxed_engine`] change.
#[cfg(feature = "wasm-runtime")]
fn engine_config_hash(engine: &wasmtime::Engine) -> [u8; 32] {
    use sha2::Digest;
    use std::hash::Hash;

    let mut hasher = Sha256Hasher(sha2::Sha256::new());
    hasher.0.update(b"coderex-wasm-aot-v1");
    engine.precompile_compatibility_hash().hash(&mut hasher);
    hasher.0.finalize().into()
}

/// SHA-256 of the exact (pre-scanned, WAT-translated) module bytes.
#[cfg(feature = "wasm-runtime")]
fn module_sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

/// `<root>/<engine hash>/<module hash>.cwasm`.
#[cfg(feature = "wasm-runtime")]
fn cache_entry_path(
    root: &std::path::Path,
    engine_hash: &[u8; 32],
    module_hash: &[u8; 32],
) -> PathBuf {
    root.join(hex::encode(engine_hash))
        .join(format!("{}.cwasm", hex::encode(module_hash)))
}

/// Wrap serialized module bytes in the self-describing cache envelope.
///
/// The envelope makes the two checks the runtime needs explicit at read time:
/// the entry was produced by a compatible engine, and it belongs to the module
/// file whose bytes produced `module_hash`.
#[cfg(feature = "wasm-runtime")]
fn encode_cache_entry(engine_hash: &[u8; 32], module_hash: &[u8; 32], payload: &[u8]) -> Vec<u8> {
    let mut entry = Vec::with_capacity(WASM_CACHE_HEADER_LEN + payload.len());
    entry.extend_from_slice(WASM_CACHE_MAGIC);
    entry.extend_from_slice(&WASM_CACHE_FORMAT_VERSION.to_le_bytes());
    entry.extend_from_slice(engine_hash);
    entry.extend_from_slice(module_hash);
    entry.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    entry.extend_from_slice(payload);
    entry
}

/// Verify an envelope and return its serialized module bytes.
#[cfg(feature = "wasm-runtime")]
fn decode_cache_entry<'a>(
    entry: &'a [u8],
    engine_hash: &[u8; 32],
    module_hash: &[u8; 32],
) -> Result<&'a [u8], String> {
    if entry.len() < WASM_CACHE_HEADER_LEN {
        return Err("cache entry is truncated".into());
    }
    if entry[..8] != WASM_CACHE_MAGIC[..] {
        return Err("cache entry has a bad magic number".into());
    }
    let version = u32::from_le_bytes(entry[8..12].try_into().expect("4 bytes"));
    if version != WASM_CACHE_FORMAT_VERSION {
        return Err(format!(
            "cache entry uses format version {version}, expected {WASM_CACHE_FORMAT_VERSION}"
        ));
    }
    if entry[12..44] != engine_hash[..] {
        return Err("cache entry was compiled by a different engine configuration".into());
    }
    if entry[44..76] != module_hash[..] {
        return Err("cache entry does not match the module file's SHA-256".into());
    }
    let len = usize::try_from(u64::from_le_bytes(
        entry[76..84].try_into().expect("8 bytes"),
    ))
    .map_err(|_| "cache entry payload length does not fit in memory".to_string())?;
    let payload = &entry[WASM_CACHE_HEADER_LEN..];
    if payload.len() != len {
        return Err(format!(
            "cache entry payload is {} bytes, header says {len}",
            payload.len()
        ));
    }
    Ok(payload)
}

/// Try to load a cached module. A corrupt or incompatible entry is deleted and
/// treated as a miss, so the next step compiles and replaces it.
#[cfg(feature = "wasm-runtime")]
fn read_cached_module(
    engine: &wasmtime::Engine,
    engine_hash: &[u8; 32],
    module_hash: &[u8; 32],
    cache_path: &std::path::Path,
    wasm_path: &std::path::Path,
) -> Option<wasmtime::Module> {
    let entry = match std::fs::read(cache_path) {
        Ok(entry) => entry,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                cache = %cache_path.display(),
                "failed to read WASM AOT cache entry: {e}"
            );
            return None;
        }
    };
    let payload = match decode_cache_entry(&entry, engine_hash, module_hash) {
        Ok(payload) => payload,
        Err(e) => {
            tracing::warn!(
                cache = %cache_path.display(),
                "ignoring WASM AOT cache entry: {e}"
            );
            let _ = std::fs::remove_file(cache_path);
            return None;
        }
    };

    // SAFETY: the envelope verified that this payload is the `Module::serialize`
    // output of an engine with the same precompile-compatibility hash, for the
    // exact module bytes being loaded. The cache lives in the user's private
    // cache directory; a same-user process that tampers with it is already
    // inside this process's trust boundary.
    match unsafe { wasmtime::Module::deserialize(engine, payload) } {
        Ok(module) => {
            tracing::debug!(
                module = %wasm_path.display(),
                cache = %cache_path.display(),
                "loaded WASM module from the AOT cache"
            );
            Some(module)
        }
        Err(e) => {
            tracing::warn!(
                cache = %cache_path.display(),
                "ignoring incompatible WASM AOT cache entry: {e}"
            );
            let _ = std::fs::remove_file(cache_path);
            None
        }
    }
}

/// Write a cache entry atomically and prune the cache back to its size bound.
#[cfg(feature = "wasm-runtime")]
fn store_cached_module(
    cache_path: &std::path::Path,
    engine_hash: &[u8; 32],
    module_hash: &[u8; 32],
    payload: &[u8],
) -> std::io::Result<()> {
    use std::io::Write as _;

    let parent = cache_path
        .parent()
        .ok_or_else(|| std::io::Error::other("cache entry has no parent directory"))?;
    std::fs::create_dir_all(parent)?;

    let entry = encode_cache_entry(engine_hash, module_hash, payload);
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        hex::encode(module_hash),
        std::process::id()
    ));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&entry)?;
        file.sync_all()?;
    }

    if let Err(rename_error) = std::fs::rename(&tmp, cache_path) {
        let _ = std::fs::remove_file(&tmp);
        // Another process may have written the same entry first. That is a
        // success as long as a complete file is there now.
        if !cache_path.is_file() {
            return Err(rename_error);
        }
    }

    if let Some(root) = parent.parent() {
        prune_cache(root, WASM_CACHE_MAX_BYTES);
    }
    Ok(())
}

/// Delete the oldest cache entries until the cache is at or below `max_bytes`.
///
/// Only engine-hash directories (64 hex characters) are considered, so the
/// staging directories used by in-flight compilations are never pruned. The
/// most recent entry is kept even when it alone exceeds the bound, so a single
/// large module cannot turn every load into a recompile.
#[cfg(feature = "wasm-runtime")]
fn prune_cache(root: &std::path::Path, max_bytes: u64) {
    let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    collect_cache_files(root, &mut entries);

    let mut total: u64 = entries.iter().map(|(_, size, _)| *size).sum();
    if total <= max_bytes {
        return;
    }

    entries.sort_by_key(|(_, _, modified)| *modified);
    while entries.len() > 1 && total > max_bytes {
        let (path, size, _) = entries.remove(0);
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
    tracing::debug!(
        root = %root.display(),
        total_bytes = total,
        "pruned the WASM AOT cache"
    );
}

/// Collect cache files under engine-hash directories only.
#[cfg(feature = "wasm-runtime")]
fn collect_cache_files(
    root: &std::path::Path,
    entries: &mut Vec<(PathBuf, u64, std::time::SystemTime)>,
) {
    let Ok(dirs) = std::fs::read_dir(root) else {
        return;
    };
    for dir in dirs.flatten() {
        let name = dir.file_name();
        let name = name.to_string_lossy();
        if name.len() != 64 || !name.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for file in files.flatten() {
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            entries.push((file.path(), metadata.len(), modified));
        }
    }
}

/// Load the module, using the AOT cache and failing closed when no compiler is
/// available.
#[cfg(feature = "wasm-runtime")]
fn load_module(
    engine: &wasmtime::Engine,
    wasm_path: &std::path::Path,
    binary: &[u8],
    compiler: Option<WasmCompilerCommand>,
    cache_dir: &std::path::Path,
) -> Result<wasmtime::Module, String> {
    let engine_hash = engine_config_hash(engine);
    let module_hash = module_sha256(binary);
    let cache_path = cache_entry_path(cache_dir, &engine_hash, &module_hash);

    if let Some(module) =
        read_cached_module(engine, &engine_hash, &module_hash, &cache_path, wasm_path)
    {
        return Ok(module);
    }

    let Some(command) = compiler else {
        return Err(format!(
            "refusing to load WASM plugin '{}': no WASM compiler command is configured. \
             The host must inject one with \
             coderex_plugins::wasm_runtime::set_compiler_command (the CodeRex CLI points it at \
             its own executable with '{INTERNAL_WASM_COMPILE_FLAG}'). Refusing to compile \
             untrusted code in-process.",
            wasm_path.display()
        ));
    };

    let serialized = run_compile_helper(&command, binary, cache_dir)?;

    // SAFETY: the bytes were produced by `Module::serialize` in our own
    // compiler process, running the same wasmtime version and the same
    // `sandboxed_engine` configuration, and were transported through a private
    // staging directory. A mismatch (for example a stale helper binary) makes
    // deserialization fail, which is a hard error here.
    let module = unsafe { wasmtime::Module::deserialize(engine, &serialized) }.map_err(|e| {
        format!(
            "WASM compiler '{}' produced bytes this runtime cannot load (engine mismatch?): {e}",
            command.program().display()
        )
    })?;

    if let Err(e) = store_cached_module(&cache_path, &engine_hash, &module_hash, &serialized) {
        tracing::warn!(
            cache = %cache_path.display(),
            "failed to store the WASM AOT cache entry: {e}"
        );
    }
    Ok(module)
}

// ─── Out-of-process compilation ─────────────────────────────────────────────

/// Run the compiler command in a killable child and return its serialized
/// module.
///
/// The child is put in its own process group (Unix) or a Job Object with
/// `KILL_ON_JOB_CLOSE` (Windows), so a timeout kills the whole tree, not just
/// the direct child. Where the platform sandbox primitives fit, they wrap the
/// command as well; see [`wrap_with_macos_sandbox`] and
/// [`wrap_with_bubblewrap`].
#[cfg(feature = "wasm-runtime")]
fn run_compile_helper(
    command: &WasmCompilerCommand,
    binary: &[u8],
    cache_root: &std::path::Path,
) -> Result<Vec<u8>, String> {
    // Stage under the cache root rather than the system temp directory:
    // bubblewrap replaces `/tmp` with a fresh tmpfs, which would make a bind
    // mount of a staging path under `/tmp` resolve against the empty tmpfs.
    let staging_root = cache_root.join("staging");
    std::fs::create_dir_all(&staging_root)
        .map_err(|e| format!("failed to create the WASM compile staging directory: {e}"))?;
    let dir = tempfile::Builder::new()
        .prefix("compile-")
        .tempdir_in(&staging_root)
        .map_err(|e| format!("failed to create a temp dir for WASM compilation: {e}"))?;
    // Sandbox profiles match the kernel's resolved paths, so canonicalize
    // (`/tmp` is a symlink to `/private/tmp` on macOS).
    let work_dir = std::fs::canonicalize(dir.path()).unwrap_or_else(|_| dir.path().to_path_buf());
    let input_path = work_dir.join("input.wasm");
    let output_path = work_dir.join("output.cwasm");
    let stderr_path = work_dir.join("helper.stderr");

    std::fs::write(&input_path, binary)
        .map_err(|e| format!("failed to stage WASM input for compilation: {e}"))?;
    let stderr = std::fs::File::create(&stderr_path)
        .map_err(|e| format!("failed to create WASM compiler stderr file: {e}"))?;

    // Mutated only by the Linux/macOS sandbox wrappers below.
    #[allow(unused_mut)]
    let mut program: OsString = command.program().as_os_str().to_os_string();
    let mut args: Vec<OsString> = command.args().to_vec();
    args.push(input_path.into_os_string());
    args.push(output_path.clone().into_os_string());

    #[cfg(target_os = "linux")]
    let _wrapper = wrap_with_bubblewrap(&mut program, &mut args, &work_dir);
    #[cfg(target_os = "macos")]
    let _sandbox = wrap_with_macos_sandbox(&mut program, &mut args, &work_dir, command.program());

    let mut child_command = std::process::Command::new(&program);
    child_command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A new process group is what makes `kill(-pid, SIGKILL)` on timeout
        // take the whole tree with it, including wrapper processes and any
        // descendants the compiler created.
        child_command.process_group(0);
        // SAFETY: `pre_exec` runs between fork and exec in the child. The
        // closure only calls `setrlimit`, which is async-signal-safe, and
        // captures two integers.
        unsafe {
            child_command.pre_exec(apply_compile_rlimits);
        }
    }

    // Windows: the compiler is confined by the shared AppContainer sandbox,
    // which also owns the kill-on-close Job Object. `prepare` creates the
    // child suspended; `post_spawn` attaches the token, assigns the job and
    // resumes it.
    #[cfg(target_os = "windows")]
    let sandbox = {
        use coderex_core::sandbox::{SandboxConfig, SandboxHandle};

        let compiler = std::fs::canonicalize(command.program())
            .unwrap_or_else(|_| command.program().to_path_buf());
        let mut read = vec![work_dir.clone(), compiler.clone()];
        let mut exec = vec![compiler.clone()];
        if let Some(parent) = compiler.parent() {
            read.push(parent.to_path_buf());
            exec.push(parent.to_path_buf());
        }
        let mut write = vec![work_dir.clone()];
        // Debug test hooks write observation files outside the staging
        // directory. Name those directories explicitly rather than disabling
        // the sandbox for the test.
        #[cfg(debug_assertions)]
        for key in [
            "CODEREX_WASM_COMPILE_TEST_COUNT_FILE",
            "CODEREX_WASM_COMPILE_TEST_CHILD_PID_FILE",
        ] {
            if let Some(path) = std::env::var_os(key) {
                if let Some(parent) = std::path::Path::new(&path).parent() {
                    write.push(parent.to_path_buf());
                }
            }
        }

        let handle = SandboxHandle::create(SandboxConfig {
            allowed_read_paths: read,
            allowed_write_paths: write,
            allowed_exec_paths: exec,
            additional_working_dirs: vec![work_dir.clone()],
            max_open_files: None,
            max_memory_bytes: Some(WASM_COMPILE_MEMORY_BYTES),
            max_cpu_seconds: Some(WASM_COMPILE_CPU_SECONDS),
            allow_network: false,
        })
        .map_err(|e| format!("failed to create the WASM compile sandbox: {e}"))?;
        handle
            .prepare(&mut child_command)
            .map_err(|e| format!("failed to prepare the WASM compile sandbox: {e}"))?;
        handle
    };

    let mut child = child_command.spawn().map_err(|e| {
        format!(
            "failed to spawn WASM compiler '{}': {e}",
            command.program().display()
        )
    })?;

    #[cfg(target_os = "windows")]
    sandbox.post_spawn(&child).map_err(|e| {
        let _ = child.kill();
        format!("failed to confine the WASM compiler: {e}")
    })?;

    #[cfg(unix)]
    let killer = ChildKiller::Group;
    #[cfg(target_os = "windows")]
    let killer = ChildKiller::Sandbox(&sandbox);
    #[cfg(not(any(unix, windows)))]
    let killer = ChildKiller::Plain;

    let status = wait_for_compile_child(&mut child, compile_timeout(), |child| killer.kill(child))?;
    if !status.success() {
        let detail = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            format!(
                "WASM compiler '{}' failed with {status}",
                command.program().display()
            )
        } else {
            format!(
                "WASM compiler '{}' failed with {status}: {detail}",
                command.program().display()
            )
        });
    }

    let serialized = std::fs::read(&output_path).map_err(|e| {
        format!(
            "WASM compiler '{}' exited successfully but produced no output: {e}",
            command.program().display()
        )
    })?;
    if serialized.is_empty() {
        return Err(format!(
            "WASM compiler '{}' produced an empty serialized module",
            command.program().display()
        ));
    }
    Ok(serialized)
}

/// Run the compiler under macOS `sandbox-exec` when the core sandbox can build
/// a profile for it.
///
/// The profile allows reading and writing the staging directory, executing the
/// compiler, and the system roots the dynamic loader needs; it denies network
/// access. It is inherited by the compiler's descendants. When sandbox-exec is
/// unavailable or profile creation fails, the command runs with rlimits and
/// the process-group kill only, and the caller's documentation says so.
#[cfg(all(target_os = "macos", feature = "wasm-runtime"))]
fn wrap_with_macos_sandbox(
    program: &mut OsString,
    args: &mut Vec<OsString>,
    work_dir: &std::path::Path,
    compiler: &std::path::Path,
) -> Option<coderex_core::sandbox::SandboxHandle> {
    use coderex_core::sandbox::{SandboxConfig, SandboxHandle};

    // Test hook: the leading-argument forwarding test uses a shell script
    // wrapper, which the compiler profile intentionally does not execute.
    #[cfg(debug_assertions)]
    if std::env::var_os("CODEREX_WASM_COMPILE_TEST_NO_SANDBOX").is_some() {
        return None;
    }

    if !std::path::Path::new("/usr/bin/sandbox-exec").is_file() {
        tracing::warn!(
            "sandbox-exec is not available; compiling WASM with rlimits and the process-group \
             kill but no filesystem or network sandbox"
        );
        return None;
    }

    let compiler = std::fs::canonicalize(compiler).unwrap_or_else(|_| compiler.to_path_buf());
    let mut allowed_read_paths = vec![work_dir.to_path_buf(), compiler.clone()];
    if let Some(parent) = compiler.parent() {
        allowed_read_paths.push(parent.to_path_buf());
    }
    let mut allowed_exec_paths = vec![compiler];
    // Debug builds add the orchestrator's child-spawn test hook (`sleep`).
    #[cfg(debug_assertions)]
    {
        allowed_exec_paths.push(PathBuf::from("/bin/sleep"));
        allowed_exec_paths.push(PathBuf::from("/usr/bin/sleep"));
    }

    let config = SandboxConfig {
        allowed_read_paths,
        allowed_write_paths: vec![work_dir.to_path_buf()],
        allowed_exec_paths,
        additional_working_dirs: vec![work_dir.to_path_buf()],
        max_cpu_seconds: Some(WASM_COMPILE_CPU_SECONDS),
        max_memory_bytes: Some(WASM_COMPILE_MEMORY_BYTES),
        allow_network: false,
        ..SandboxConfig::default()
    };

    let handle = match SandboxHandle::create(config) {
        Ok(handle) => handle,
        Err(e) => {
            tracing::warn!(
                "failed to create the macOS sandbox for WASM compilation ({e}); continuing with \
                 rlimits and the process-group kill only"
            );
            return None;
        }
    };
    let (wrapper, wrapper_args) = handle.sandboxed_program()?;

    let inner_program = program.clone();
    let inner_args = std::mem::take(args);
    let mut wrapped: Vec<OsString> = wrapper_args.into_iter().map(OsString::from).collect();
    wrapped.push(inner_program);
    wrapped.extend(inner_args);
    *program = OsString::from(wrapper);
    *args = wrapped;
    Some(handle)
}

/// Run the compiler inside a bubblewrap mount/network namespace when `bwrap`
/// is in force.
///
/// The root is read-only, the staging directory is the only writable path, and
/// the network namespace is unshared. The core seccomp allowlist is *not*
/// applied: it is written for shells and would reject wasmtime's syscalls.
/// rlimits and the process-group kill still apply through `pre_exec`.
#[cfg(all(target_os = "linux", feature = "wasm-runtime"))]
fn wrap_with_bubblewrap(
    program: &mut OsString,
    args: &mut Vec<OsString>,
    work_dir: &std::path::Path,
) -> bool {
    use coderex_core::sandbox::{SandboxConfig, bubblewrap_program};

    // `bubblewrap_args` replaces `/tmp` with a fresh tmpfs. A staging
    // directory under `/tmp` would then resolve inside that empty tmpfs and
    // the writable bind would fail, so fall back to rlimits for that layout
    // instead of claiming a sandbox that cannot be applied.
    let system_temp =
        std::fs::canonicalize(std::env::temp_dir()).unwrap_or_else(|_| std::env::temp_dir());
    if work_dir.starts_with(&system_temp) {
        tracing::warn!(
            staging = %work_dir.display(),
            "WASM compilation staging is inside the system temp directory, which bubblewrap \
             replaces; compiling with rlimits and the process-group kill but no bubblewrap"
        );
        return false;
    }

    let config = SandboxConfig {
        allowed_write_paths: vec![work_dir.to_path_buf()],
        additional_working_dirs: vec![work_dir.to_path_buf()],
        max_cpu_seconds: Some(WASM_COMPILE_CPU_SECONDS),
        max_memory_bytes: Some(WASM_COMPILE_MEMORY_BYTES),
        allow_network: false,
        ..SandboxConfig::default()
    };
    let Some((wrapper, wrapper_args)) = bubblewrap_program(&config) else {
        return false;
    };

    let inner_program = program.clone();
    let inner_args = std::mem::take(args);
    let mut wrapped: Vec<OsString> = wrapper_args.into_iter().map(OsString::from).collect();
    wrapped.push(inner_program);
    wrapped.extend(inner_args);
    *program = OsString::from(wrapper);
    *args = wrapped;
    true
}

/// Which resource bound `apply_compile_rlimits` should install for memory.
///
/// Kept as an enum so the platform choice is visible to the unit tests instead
/// of being buried in a `cfg`-selected `libc` constant.
#[cfg(all(unix, feature = "wasm-runtime"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Exactly one variant is constructed on a given platform; the other is still
// part of the plan type and is asserted by the tests' platform branch.
#[allow(dead_code)]
enum CompileMemoryRlimit {
    /// `RLIMIT_AS` — total virtual address space (Linux and other Unix).
    AddressSpace,
    /// `RLIMIT_DATA` — the data segment (Darwin, which rejects `RLIMIT_AS`).
    Data,
}

#[cfg(all(unix, feature = "wasm-runtime"))]
impl CompileMemoryRlimit {
    /// The `libc` resource constant to pass to `setrlimit`.
    const fn resource(self) -> libc::c_int {
        match self {
            Self::AddressSpace => libc::RLIMIT_AS,
            Self::Data => libc::RLIMIT_DATA,
        }
    }
}

/// The resource used for the compiler child's memory bound on this platform.
///
/// Darwin returns `EINVAL` for a finite `RLIMIT_AS`, so it gets `RLIMIT_DATA`
/// instead. `RLIMIT_DATA` bounds the data segment, not the whole address
/// space: it is a useful ceiling for an allocator-heavy compiler, but it is
/// **not** a full address-space cap (mappings the process makes directly, its
/// stack and its executable image are outside it). Other Unix platforms keep
/// `RLIMIT_AS`.
#[cfg(all(unix, feature = "wasm-runtime"))]
const fn compile_memory_rlimit() -> CompileMemoryRlimit {
    #[cfg(target_os = "macos")]
    {
        CompileMemoryRlimit::Data
    }
    #[cfg(not(target_os = "macos"))]
    {
        CompileMemoryRlimit::AddressSpace
    }
}

/// The CPU and memory limits installed on the compiler child.
///
/// Split out from `apply_compile_rlimits` so the values and the platform's
/// memory-resource choice can be asserted by a unit test (`pre_exec` itself
/// cannot be called from a test).
#[cfg(all(unix, feature = "wasm-runtime"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompileRlimitPlan {
    cpu_seconds: u64,
    memory_rlimit: CompileMemoryRlimit,
    memory_bytes: u64,
}

#[cfg(all(unix, feature = "wasm-runtime"))]
const fn compile_rlimit_plan() -> CompileRlimitPlan {
    CompileRlimitPlan {
        cpu_seconds: WASM_COMPILE_CPU_SECONDS,
        memory_rlimit: compile_memory_rlimit(),
        memory_bytes: WASM_COMPILE_MEMORY_BYTES,
    }
}

/// Install the compiler child's CPU and memory limits.
///
/// `RLIMIT_CPU` is mandatory: without it this process has no hard CPU bound.
/// The memory limit is best-effort. On Linux it is `RLIMIT_AS`; on Darwin it
/// is `RLIMIT_DATA`, because Darwin rejects finite address-space limits with
/// `EINVAL`. `RLIMIT_DATA` is not a full address-space cap (see
/// [`compile_memory_rlimit`]). A failure here cannot be logged (`pre_exec` is
/// not async-signal-safe), so the documentation carries the caveat; the child
/// is still bounded by CPU time and by the parent's wall-clock kill.
#[cfg(all(unix, feature = "wasm-runtime"))]
fn apply_compile_rlimits() -> std::io::Result<()> {
    use libc::{RLIMIT_CPU, rlimit, setrlimit};

    let plan = compile_rlimit_plan();

    let cpu = rlimit {
        rlim_cur: plan.cpu_seconds,
        rlim_max: plan.cpu_seconds,
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
        rlim_cur: plan.memory_bytes,
        rlim_max: plan.memory_bytes,
    };
    // SAFETY: as above. Failure is deliberately ignored: see the doc comment.
    let _ = unsafe { setrlimit(plan.memory_rlimit.resource(), std::ptr::from_ref(&memory)) };

    Ok(())
}

/// How a timed-out compiler child is killed.
#[cfg(all(unix, feature = "wasm-runtime"))]
enum ChildKiller {
    /// The child leads its own process group (`process_group(0)`).
    Group,
}

#[cfg(all(unix, feature = "wasm-runtime"))]
impl ChildKiller {
    fn kill(&self, child: &mut std::process::Child) {
        match self {
            Self::Group => {
                if let Ok(pid) = libc::pid_t::try_from(child.id()) {
                    // SAFETY: `process_group(0)` made the child the leader of
                    // a new group and it has not exec'd anything that changes
                    // that. `-pid` therefore names exactly the spawned tree.
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                }
                let _ = child.kill();
            }
        }
    }
}

#[cfg(all(windows, feature = "wasm-runtime"))]
enum ChildKiller<'a> {
    /// The child and every descendant live in this sandbox's Job Object.
    Sandbox(&'a coderex_core::sandbox::SandboxHandle),
}

#[cfg(all(windows, feature = "wasm-runtime"))]
impl ChildKiller<'_> {
    fn kill(&self, child: &mut std::process::Child) {
        match self {
            Self::Sandbox(sandbox) => {
                let _ = sandbox.terminate();
                let _ = child.kill();
            }
        }
    }
}

#[cfg(all(not(any(unix, windows)), feature = "wasm-runtime"))]
enum ChildKiller {
    Plain,
}

#[cfg(all(not(any(unix, windows)), feature = "wasm-runtime"))]
impl ChildKiller {
    fn kill(&self, child: &mut std::process::Child) {
        let _ = child.kill();
    }
}

/// Wait for the compiler child, killing its whole tree when the wall-clock
/// budget expires.
#[cfg(feature = "wasm-runtime")]
fn wait_for_compile_child(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
    killer: impl Fn(&mut std::process::Child),
) -> Result<std::process::ExitStatus, String> {
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if started.elapsed() >= timeout {
                    killer(child);
                    let _ = child.wait();
                    return Err(format!(
                        "WASM compilation exceeded the {} ms wall-clock limit; the helper process \
                         tree was killed",
                        timeout.as_millis()
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => {
                killer(child);
                let _ = child.wait();
                return Err(format!("failed to wait for the WASM compile helper: {e}"));
            }
        }
    }
}

/// Compile a WASM module and return wasmtime's serialized representation.
///
/// Entry point for the hidden compiler process (the `coderex-wasm-compile`
/// helper binary and the CLI's `--internal-wasm-compile` argument); not part
/// of the stable plugin API. The pre-scan runs here too, so a directly invoked
/// compiler still refuses pathological input.
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
    // SAFETY: this test hook trusts its caller the same way the load path
    // trusts the output of its own compiler process.
    unsafe { wasmtime::Module::deserialize(&engine, bytes) }
        .map(|_| ())
        .map_err(|e| format!("failed to deserialize compiled WASM module: {e}"))
}

/// Run the hidden compiler protocol: `<args> <input> <output>`.
///
/// Both the `coderex-wasm-compile` helper binary and the CLI's internal
/// subcommand call this. It reads the input file, pre-scans and compiles it,
/// and writes `Module::serialize` bytes to the output path.
#[cfg(feature = "wasm-runtime")]
#[doc(hidden)]
pub fn compile_helper_main<I>(args: I) -> Result<(), String>
where
    I: IntoIterator<Item = OsString>,
{
    #[cfg(debug_assertions)]
    compiler_test_hooks::before_compile();

    let mut args = args.into_iter();
    let (Some(input), Some(output)) = (args.next(), args.next()) else {
        return Err("usage: <input> <output>".into());
    };
    if args.next().is_some() {
        return Err("usage: <input> <output>".into());
    }

    let bytes = std::fs::read(&input)
        .map_err(|e| format!("failed to read '{}': {e}", input.to_string_lossy()))?;
    let serialized = compile_to_serialized(&bytes)?;
    std::fs::write(&output, serialized)
        .map_err(|e| format!("failed to write '{}': {e}", output.to_string_lossy()))?;
    Ok(())
}

/// Stub when the WASM runtime is compiled out.
#[cfg(not(feature = "wasm-runtime"))]
#[doc(hidden)]
pub fn compile_helper_main<I>(_args: I) -> Result<(), String>
where
    I: IntoIterator<Item = OsString>,
{
    Err("WASM runtime not enabled. Rebuild with --features wasm-runtime".into())
}

/// Debug-build hooks used by the subprocess tests.
///
/// They are inert unless the corresponding environment variable is set, and
/// they are compiled out of release builds entirely.
#[cfg(all(feature = "wasm-runtime", debug_assertions))]
mod compiler_test_hooks {
    use std::io::Write as _;

    pub fn before_compile() {
        if let Ok(path) = std::env::var("CODEREX_WASM_COMPILE_TEST_COUNT_FILE") {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "compile");
            }
        }

        // Spawn a descendant that outlives the compiler unless the parent's
        // timeout kill takes the whole process group / job.
        #[cfg(unix)]
        if let Ok(path) = std::env::var("CODEREX_WASM_COMPILE_TEST_CHILD_PID_FILE") {
            if let Ok(child) = std::process::Command::new("sleep")
                .arg("60")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                let _ = std::fs::write(path, child.id().to_string());
                // Dropping `Child` does not kill it; it runs until the group
                // kill or its own `sleep` ends.
            }
        }

        #[cfg(target_os = "windows")]
        if let Ok(path) = std::env::var("CODEREX_WASM_COMPILE_TEST_CHILD_PID_FILE") {
            if let Ok(child) = std::process::Command::new("cmd.exe")
                .args(["/C", "ping -n 60 127.0.0.1 > nul"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                let _ = std::fs::write(path, child.id().to_string());
                // Dropping `Child` does not kill it; it runs until the job is
                // terminated or its own `ping` ends.
            }
        }

        if let Ok(raw) = std::env::var("CODEREX_WASM_COMPILE_TEST_SLEEP_MS") {
            if let Ok(millis) = raw.parse::<u64>() {
                std::thread::sleep(std::time::Duration::from_millis(millis));
            }
        }
    }
}

/// Module name of the `user_config` host import.
#[cfg(feature = "wasm-runtime")]
const USER_CONFIG_MODULE: &str = "coderex";

/// Function name of the `user_config` host import.
#[cfg(feature = "wasm-runtime")]
const USER_CONFIG_FUNCTION: &str = "user_config";

/// Per-call store data: the resource limiter plus the `userConfig` JSON.
#[cfg(feature = "wasm-runtime")]
struct StoreState {
    limits: wasmtime::StoreLimits,
    user_config: Arc<[u8]>,
}

/// The store type used by every plugin call.
#[cfg(feature = "wasm-runtime")]
type PluginStore = wasmtime::Store<StoreState>;

/// Implement `coderex.user_config(ptr, capacity) -> required_len`.
///
/// The host copies the serialized options object into guest memory at `ptr`,
/// writing at most `capacity` bytes, and returns the full JSON length. A guest
/// can therefore:
/// 1. call with `(0, 0)` to learn the length,
/// 2. allocate a buffer of that size and call again,
/// 3. treat `returned > capacity` as truncation.
///
/// Returns `-1` for a negative pointer/capacity, a missing `memory` export, or
/// a write that would leave guest memory. `memory` is the module's exported
/// linear memory, which the plugin ABI already requires.
#[cfg(feature = "wasm-runtime")]
fn write_user_config(
    caller: &mut wasmtime::Caller<'_, StoreState>,
    ptr: i32,
    capacity: i32,
) -> i32 {
    if ptr < 0 || capacity < 0 {
        return -1;
    }

    let json = Arc::clone(&caller.data().user_config);
    let Ok(required) = i32::try_from(json.len()) else {
        return -1;
    };

    // A zero-capacity call is the length query, and needs no memory export.
    if capacity == 0 {
        return required;
    }

    let Some(memory) = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
    else {
        return -1;
    };
    let data = memory.data_mut(caller);
    let Ok(offset) = usize::try_from(ptr) else {
        return -1;
    };
    let Ok(capacity) = usize::try_from(capacity) else {
        return -1;
    };
    if offset >= data.len() {
        return -1;
    }
    let to_write = json.len().min(capacity);
    let Some(end) = offset.checked_add(to_write) else {
        return -1;
    };
    if end > data.len() {
        return -1;
    }
    data[offset..end].copy_from_slice(&json[..to_write]);
    required
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
    /// The resolved `userConfig` options as a JSON object, served to the guest
    /// through the `coderex.user_config` import. Empty modules never see it;
    /// modules that import the function do.
    #[cfg(feature = "wasm-runtime")]
    user_config: Arc<[u8]>,
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
    /// Load a WASM module from the given path with no `userConfig` options.
    ///
    /// The file is rejected before compilation if it is larger than
    /// `PLUGIN_MAX_WASM_BYTES`. The metadata check avoids reading an oversized
    /// file into memory at all; the length is checked again after reading in
    /// case the file changed between the two calls.
    ///
    /// The module is loaded from the AOT cache when a compatible entry exists;
    /// otherwise it is compiled by the host-injected command (or a helper next
    /// to the executable). With neither available, loading **fails closed** —
    /// this function never compiles untrusted code in-process.
    pub fn new(wasm_path: &std::path::Path) -> Result<Self, String> {
        Self::load_with(wasm_path, resolved_compiler_command(), None)
    }

    /// Load a WASM module and make resolved `userConfig` options available to
    /// it through the `coderex.user_config` import.
    ///
    /// `user_config` holds the same values a process plugin receives as
    /// `CODEREX_PLUGIN_OPTION_<KEY>`: one entry per resolved option, with
    /// absent optional options simply missing. The values are encoded as one
    /// JSON object; a plugin receives them only if it imports and calls
    /// `coderex.user_config`. A module with no imports loads and runs exactly
    /// as before.
    pub fn new_with_options(
        wasm_path: &std::path::Path,
        user_config: HashMap<String, String>,
    ) -> Result<Self, String> {
        Self::load_with_options(wasm_path, resolved_compiler_command(), None, user_config)
    }

    /// Load with an explicit compiler command and cache directory.
    ///
    /// `compiler: None` disables compilation for this call, which makes the
    /// fail-closed path testable. `cache_dir: None` uses the resolved default.
    /// This is an internal seam; hosts configure the runtime with
    /// [`set_compiler_command`] and call [`PluginWasmInstance::new`].
    #[doc(hidden)]
    pub fn load_with(
        wasm_path: &std::path::Path,
        compiler: Option<WasmCompilerCommand>,
        cache_dir: Option<PathBuf>,
    ) -> Result<Self, String> {
        Self::load_with_options(wasm_path, compiler, cache_dir, HashMap::new())
    }

    /// [`PluginWasmInstance::new_with_options`] with explicit compiler and
    /// cache-directory seams. Internal; used by tests.
    #[doc(hidden)]
    pub fn load_with_options(
        wasm_path: &std::path::Path,
        compiler: Option<WasmCompilerCommand>,
        cache_dir: Option<PathBuf>,
        user_config: HashMap<String, String>,
    ) -> Result<Self, String> {
        // BTreeMap gives the JSON a stable key order, so the same options
        // always produce the same bytes (useful for tests and diagnostics).
        let user_config: std::collections::BTreeMap<_, _> = user_config.into_iter().collect();
        let user_config: Arc<[u8]> = serde_json::to_vec(&user_config)
            .map_err(|e| format!("failed to serialize WASM userConfig: {e}"))?
            .into();

        let engine = sandboxed_engine()?;

        let metadata = std::fs::metadata(wasm_path)
            .map_err(|e| format!("failed to read WASM module '{}': {e}", wasm_path.display()))?;
        ensure_within_size_cap(wasm_path, metadata.len())?;
        let bytes = std::fs::read(wasm_path)
            .map_err(|e| format!("failed to read WASM module '{}': {e}", wasm_path.display()))?;
        ensure_within_size_cap(wasm_path, bytes.len() as u64)?;

        // The linear pre-scan is the first gate: it runs before any compiler
        // process is started, and the compiler repeats it for the bytes it
        // receives.
        let name = wasm_path.display().to_string();
        let scanned = prescan_module(&name, &bytes, PrescanLimits::default())?;

        let cache_dir = cache_dir.unwrap_or_else(resolved_cache_dir);
        let module = load_module(&engine, wasm_path, &scanned, compiler, &cache_dir)?;
        Ok(Self {
            engine,
            module,
            user_config,
            initialized: false,
            commands: Vec::new(),
            skills: Vec::new(),
        })
    }

    /// Create a fresh, resource-limited `Store` for a single plugin call.
    ///
    /// Every call gets its own store, so the fuel budget and memory ceiling are
    /// reset per call rather than accumulating across calls.
    fn new_store(&self) -> Result<PluginStore, String> {
        let limits = wasmtime::StoreLimitsBuilder::new()
            .memory_size(PLUGIN_MAX_MEMORY_BYTES)
            .build();
        let mut store = wasmtime::Store::new(
            &self.engine,
            StoreState {
                limits,
                user_config: Arc::clone(&self.user_config),
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(PLUGIN_FUEL)
            .map_err(|e| format!("failed to set WASM fuel: {e}"))?;
        Ok(store)
    }

    /// Build a linker with the one host interface a plugin may import.
    ///
    /// The function is defined unconditionally: a module that does not import
    /// it is instantiated with an empty import set exactly as before. WASI is
    /// still absent, so a module importing anything else fails to instantiate.
    fn linker(&self) -> Result<wasmtime::Linker<StoreState>, String> {
        let mut linker = wasmtime::Linker::new(&self.engine);
        linker
            .func_wrap(
                USER_CONFIG_MODULE,
                USER_CONFIG_FUNCTION,
                |mut caller: wasmtime::Caller<'_, StoreState>, ptr: i32, capacity: i32| -> i32 {
                    write_user_config(&mut caller, ptr, capacity)
                },
            )
            .map_err(|e| format!("failed to define the WASM user_config import: {e}"))?;
        Ok(linker)
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
    pub fn initialize(&mut self) -> Result<serde_json::Value, String> {
        let config = serde_json::json!({
            "protocolVersion": "1.0",
            "capabilities": {},
            "clientInfo": {
                "name": "coderex",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        let result = self.call_with_json("coderex_initialize", Some(&config.to_string()))?;
        self.initialized = true;
        Ok(result)
    }

    /// Discover commands from the plugin.
    pub fn list_commands(&mut self) -> Result<Vec<PluginCommandInfo>, String> {
        let result = self.call_with_json("coderex_commands_list", None)?;
        let commands: Vec<PluginCommandInfo> = serde_json::from_value(result)
            .map_err(|e| format!("failed to parse commands/list response: {e}"))?;
        self.commands.clone_from(&commands);
        Ok(commands)
    }

    /// Discover skills from the plugin.
    pub fn list_skills(&mut self) -> Result<Vec<PluginSkillInfo>, String> {
        let result = self.call_with_json("coderex_skills_list", None)?;
        let skills: Vec<PluginSkillInfo> = serde_json::from_value(result)
            .map_err(|e| format!("failed to parse skills/list response: {e}"))?;
        self.skills.clone_from(&skills);
        Ok(skills)
    }

    /// Execute a plugin command.
    pub fn execute_command(&mut self, name: &str, args: &[String]) -> Result<String, String> {
        let args_json =
            serde_json::to_string(args).map_err(|e| format!("failed to serialize args: {e}"))?;
        let result = self.call_with_two_strings("coderex_command_execute", name, &args_json)?;
        match result {
            serde_json::Value::String(s) => Ok(s),
            other => Ok(other.to_string()),
        }
    }

    /// Execute a plugin skill.
    pub fn execute_skill(
        &mut self,
        name: &str,
        context: &serde_json::Value,
    ) -> Result<String, String> {
        let ctx_json = context.to_string();
        let result = self.call_with_two_strings("coderex_skill_execute", name, &ctx_json)?;
        match result {
            serde_json::Value::String(s) => Ok(s),
            other => Ok(other.to_string()),
        }
    }

    /// Internal helper: call a guest function that takes an optional JSON
    /// string and returns a JSON string pointer.
    fn call_with_json(
        &self,
        method: &str,
        params_json: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        tokio::task::block_in_place(|| {
            let mut store = self.new_store()?;
            let linker = self.linker()?;
            let instance = linker
                .instantiate(&mut store, &self.module)
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
            let result_len = i32::try_from(result_json.len())
                .map_err(|_| "WASM result exceeds i32 length".to_string())?;
            let _ = dealloc.call(&mut store, (result_ptr, result_len));

            serde_json::from_str(&result_json)
                .map_err(|e| format!("failed to parse WASM JSON result: {e}"))
        })
    }

    /// Internal helper: call a guest function that takes two strings
    /// `(ptr1, len1, ptr2, len2) -> i32`.
    fn call_with_two_strings(
        &self,
        method: &str,
        s1: &str,
        s2: &str,
    ) -> Result<serde_json::Value, String> {
        tokio::task::block_in_place(|| {
            let mut store = self.new_store()?;
            let linker = self.linker()?;
            let instance = linker
                .instantiate(&mut store, &self.module)
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
            let result_len = i32::try_from(result_json.len())
                .map_err(|_| "WASM result exceeds i32 length".to_string())?;
            let _ = dealloc.call(&mut store, (result_ptr, result_len));

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
    pub fn initialize(&mut self) -> Result<serde_json::Value, String> {
        Err("WASM runtime not enabled".into())
    }

    /// Returns an error when the feature is disabled.
    pub fn list_commands(&mut self) -> Result<Vec<PluginCommandInfo>, String> {
        Err("WASM runtime not enabled".into())
    }

    /// Returns an error when the feature is disabled.
    pub fn list_skills(&mut self) -> Result<Vec<PluginSkillInfo>, String> {
        Err("WASM runtime not enabled".into())
    }

    /// Returns an error when the feature is disabled.
    pub fn execute_command(&mut self, _name: &str, _args: &[String]) -> Result<String, String> {
        Err("WASM runtime not enabled".into())
    }

    /// Returns an error when the feature is disabled.
    pub fn execute_skill(
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
    store: &mut PluginStore,
    alloc: &wasmtime::TypedFunc<i32, i32>,
    data: &str,
) -> Result<(i32, i32), String> {
    let bytes = data.as_bytes();
    let len =
        i32::try_from(bytes.len()).map_err(|_| "WASM string exceeds i32 length".to_string())?;
    let ptr = alloc
        .call(&mut *store, len)
        .map_err(|e| format!("alloc failed: {e}"))?;
    let mem_data = memory.data_mut(store);
    let offset =
        usize::try_from(ptr).map_err(|_| "WASM alloc returned a negative pointer".to_string())?;
    if offset + bytes.len() > mem_data.len() {
        return Err("WASM alloc returned pointer outside memory bounds".into());
    }
    mem_data[offset..offset + bytes.len()].copy_from_slice(bytes);
    Ok((ptr, len))
}

#[cfg(feature = "wasm-runtime")]
fn read_cstring(
    memory: &wasmtime::Memory,
    store: &wasmtime::Store<StoreState>,
    ptr: i32,
) -> Result<String, String> {
    if ptr == 0 {
        return Err("WASM returned null pointer".into());
    }
    let offset =
        usize::try_from(ptr).map_err(|_| "WASM returned a negative pointer".to_string())?;
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

    /// The injected command keeps its program and leading arguments distinct;
    /// the runtime appends `<input> <output>` at execution time.
    #[test]
    fn compiler_command_keeps_program_and_leading_args() {
        let command = WasmCompilerCommand::new("/usr/bin/example")
            .arg("--internal-wasm-compile")
            .arg("extra");
        assert_eq!(command.program(), std::path::Path::new("/usr/bin/example"));
        assert_eq!(
            command.args(),
            [
                OsString::from("--internal-wasm-compile"),
                OsString::from("extra"),
            ]
        );
    }

    /// The compiler child's limit plan is fixed: hard CPU seconds and a 2 GiB
    /// memory bound. Darwin must select `RLIMIT_DATA` (it rejects a finite
    /// `RLIMIT_AS`); every other Unix selects `RLIMIT_AS`. `pre_exec` cannot
    /// be invoked from a test, so this asserts the plan it installs.
    #[cfg(all(feature = "wasm-runtime", unix))]
    #[test]
    fn compile_rlimit_plan_matches_the_platform() {
        let plan = compile_rlimit_plan();

        assert_eq!(plan.cpu_seconds, WASM_COMPILE_CPU_SECONDS);
        assert_eq!(plan.cpu_seconds, 20);
        assert_eq!(plan.memory_bytes, WASM_COMPILE_MEMORY_BYTES);
        assert_eq!(plan.memory_bytes, 2 * 1024 * 1024 * 1024);

        #[cfg(target_os = "macos")]
        {
            assert_eq!(plan.memory_rlimit, CompileMemoryRlimit::Data);
            assert_eq!(plan.memory_rlimit.resource(), libc::RLIMIT_DATA);
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(plan.memory_rlimit, CompileMemoryRlimit::AddressSpace);
            assert_eq!(plan.memory_rlimit.resource(), libc::RLIMIT_AS);
        }
    }

    /// Debug builds resolve the helper binary shipped next to the tests, which
    /// is what lets `cargo test -p coderex-plugins` exercise the real
    /// compiler without configuring a command explicitly.
    #[cfg(all(feature = "wasm-runtime", debug_assertions))]
    #[test]
    fn helper_binary_is_discovered_next_to_the_test_binary() {
        let helper = helper_next_to_current_exe().expect("helper must be discoverable");
        assert!(
            helper
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(WASM_COMPILE_HELPER)),
            "unexpected helper path: {}",
            helper.display()
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

    /// The real helper binary built next to the test binary.
    fn test_compiler() -> WasmCompilerCommand {
        let helper = helper_next_to_current_exe()
            .expect("the coderex-wasm-compile helper must be built next to the test binary");
        WasmCompilerCommand::new(helper)
    }

    /// Load through the injected-command path with a private cache directory.
    fn load(path: &Path) -> Result<PluginWasmInstance, String> {
        let cache = tempfile::tempdir().expect("failed to create a test cache dir");
        PluginWasmInstance::load_with(
            path,
            Some(test_compiler()),
            Some(cache.path().to_path_buf()),
        )
    }

    /// Load with resolved `userConfig` options through the same seams.
    fn load_with_options(
        path: &Path,
        options: &[(&str, &str)],
    ) -> Result<PluginWasmInstance, String> {
        let cache = tempfile::tempdir().expect("failed to create a test cache dir");
        let options: std::collections::HashMap<String, String> = options
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        PluginWasmInstance::load_with_options(
            path,
            Some(test_compiler()),
            Some(cache.path().to_path_buf()),
            options,
        )
    }

    /// A module that imports `coderex.user_config` receives the resolved
    /// options as one JSON object.
    ///
    /// `FILE_TOKEN` stands in for a sensitive value: at this layer the runtime
    /// carries resolved values verbatim, so a sensitive option is present. The
    /// absent optional option is the other half of the contract: it is simply
    /// missing from the object rather than present as `null` or an empty
    /// string.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_config_import_receives_resolved_options() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_module(
            dir.path(),
            "config.wasm",
            r#"(module
                 (import "coderex" "user_config"
                   (func $user_config (param i32 i32) (result i32)))
                 (memory (export "memory") 1)
                 (func (export "alloc") (param i32) (result i32)
                   i32.const 1024)
                 (func (export "dealloc") (param i32 i32))
                 (func (export "coderex_initialize") (param i32 i32) (result i32)
                   (local $required i32)
                   ;; Ask how long the JSON is without writing anything.
                   i32.const 0
                   i32.const 0
                   call $user_config
                   local.set $required
                   ;; Then write it at 4096 (capacity 1024).
                   local.get $required
                   i32.const 0
                   i32.gt_s
                   if
                     i32.const 4096
                     i32.const 1024
                     call $user_config
                     drop
                   end
                   ;; Return the JSON; the host reads it as the result.
                   i32.const 4096))"#,
        );

        let mut instance = load_with_options(
            &path,
            &[
                ("FILE_TOKEN", "s3cret"),
                ("REGION", "eu"),
                // OPTIONAL is declared by the manifest but has no value: the
                // host must not invent one.
            ],
        )
        .expect("a module importing coderex.user_config must load");

        let value = instance
            .initialize()
            .expect("initialize must return the injected options JSON");

        assert_eq!(value["FILE_TOKEN"], "s3cret");
        assert_eq!(value["REGION"], "eu");
        assert!(
            value.get("OPTIONAL").is_none(),
            "an unresolved optional option must be absent, got: {value}"
        );
        assert_eq!(
            value.as_object().map(serde_json::Map::len),
            Some(2),
            "only the resolved options must be present, got: {value}"
        );
    }

    /// Linking the one host function did not change the empty-import path: a
    /// module that imports nothing still instantiates and runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn module_without_imports_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_module(
            dir.path(),
            "no-imports.wasm",
            r#"(module
                 (memory (export "memory") 1)
                 (data (i32.const 16) "\"ok\"\00")
                 (func (export "alloc") (param i32) (result i32)
                   i32.const 1024)
                 (func (export "dealloc") (param i32 i32))
                 (func (export "coderex_initialize") (param i32 i32) (result i32)
                   i32.const 16))"#,
        );

        let mut instance = load(&path).expect("a module with no imports must load");
        let value = instance
            .initialize()
            .expect("a module with no imports must still initialize");
        assert_eq!(value, serde_json::json!("ok"));
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

        let err = load(&path).unwrap_err();
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

        let err = load(&path).unwrap_err();
        assert!(
            !err.contains("PLUGIN_MAX_WASM_BYTES"),
            "a file at the cap must not be rejected by the file-size gate: {err}"
        );
        assert!(
            err.contains("pre-scan") || err.contains("text form") || err.contains("failed to load"),
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

        assert!(load(&path).is_ok(), "a module below the size cap must load");
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

        let mut instance = load(&path).unwrap();
        let err = instance
            .initialize()
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

        let mut instance = load(&path).unwrap();
        let err = instance
            .initialize()
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

        let mut instance = load(&path).unwrap();
        let err = instance
            .initialize()
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
            load(&path).is_err(),
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

        let mut instance = load(&path).unwrap();
        let err = instance
            .initialize()
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
        let wat =
            br"(module (func nop nop nop nop nop nop nop nop nop nop nop nop nop nop nop nop))";

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

        prescan_module("at-cap.wat", wat, limits).expect("nesting at the cap must be accepted");
    }
}
