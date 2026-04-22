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
        let engine = wasmtime::Engine::default();
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
        let result = self.call_with_json("coderex_initialize", Some(&config.to_string())).await?;
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
    pub async fn execute_command(
        &mut self,
        name: &str,
        args: &[String],
    ) -> Result<String, String> {
        let args_json = serde_json::to_string(args)
            .map_err(|e| format!("failed to serialize args: {e}"))?;
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
            let mut store = wasmtime::Store::new(&self.engine, ());
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
                    .map_err(|e| format!("WASM call '{method}' failed: {e}"))?;
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
            let mut store = wasmtime::Store::new(&self.engine, ());
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
                .map_err(|e| format!("WASM call '{method}' failed: {e}"))?;

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
    store: &mut wasmtime::Store<()>,
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
    store: &wasmtime::Store<()>,
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
    String::from_utf8(bytes.to_vec())
        .map_err(|e| format!("WASM string is not valid UTF-8: {e}"))
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
