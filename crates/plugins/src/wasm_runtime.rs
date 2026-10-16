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
#[cfg(feature = "wasm-runtime")]
fn sandboxed_engine() -> Result<wasmtime::Engine, String> {
    let mut config = wasmtime::Config::new();
    config.consume_fuel(true);
    config.wasm_threads(false);
    wasmtime::Engine::new(&config).map_err(|e| format!("failed to build WASM engine: {e}"))
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
    pub fn new(wasm_path: &std::path::Path) -> Result<Self, String> {
        let engine = sandboxed_engine()?;
        let module = wasmtime::Module::from_file(&engine, wasm_path)
            .map_err(|e| format!("failed to load WASM module '{}': {e}", wasm_path.display()))?;
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
        // When wasm-runtime is off, new() returns an error.
        let result = PluginWasmInstance::new(std::path::Path::new("/nonexistent/plugin.wasm"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("not enabled") || err.contains("failed to load"),
            "unexpected error: {err}"
        );
    }
}

/// Sandbox tests. These only exist when the WASM runtime is compiled in; run
/// them with `cargo test -p coderex-plugins --features wasm-runtime`.
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
