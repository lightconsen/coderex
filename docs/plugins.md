# plugins Module Inspection

## Goal

Manages plugin lifecycle (discovery, loading, execution, unloading), marketplace connectivity (multiple marketplaces), and skill loading from multiple sources (bundled, user dir, repo dir, plugin, MCP).

## Design

### Key Types (plugins.rs)

- **`PluginState`** — Lifecycle enum: Discovered, Loading, Active, Error(String), Uninstalled, Installing.
- **`PluginHandle`** — Runtime representation with name, version, state, path, commands, skills, retry_count, max_retries.
- **`PluginManifest`** — Deserialized from `plugin.json` (name, version, description, author, min_version, commands, skills, dependencies, signature, **runtime**, **entrypoint**, **env**).
- **`RuntimeType`** — Enum: `Process` (default), `Wasm`. Declares how the plugin executes.
- **`MarketplacePlugin`** — Marketplace listing with download URL.
- **`MarketplaceConnection`** — HTTP client + base URL + configurable endpoint paths per marketplace.
- **`MarketplaceError`** — URL validation errors: InsecureUrl, InvalidUrl, NotAllowed.
- **`PluginManager`** — `DashMap`-backed registry of plugins and marketplaces with install locks, version checks, signature verification, dependency resolution, and **active process tracking**.
- **`ReconciliationReport`** — Diff between local and marketplace state.
- **`MarketplaceReconciler`** — Background reconciliation with optional auto-install.

### Key Types (runtime.rs)

- **`PluginProcess`** — JSON-RPC 2.0 stdio transport for external process plugins. Spawns a child process, performs `coderex/initialize` handshake, and exposes `list_commands`, `execute_command`, `list_skills`, `execute_skill`, `ping`.
- **`PluginCommandInfo`** — Discovered command metadata (name, description, args).
- **`PluginSkillInfo`** — Discovered skill metadata (name, description, hooks).
- **`JsonRpcRequest` / `JsonRpcResponse`** — JSON-RPC 2.0 message types.

### Key Types (command_bridge.rs)

- **`PluginCommandBridge`** — Iterates active plugin processes and registers each discovered command as a namespaced `SlashCommand` (`/{plugin}/{command}`) in the `CommandRegistry`. Handlers forward to `PluginManager::execute_command` via `tokio::task::block_in_place`.

### Key Types (tool_executor.rs)

- **`PluginToolExecutor`** — Implements `coderex_core::tool_executor::ToolExecutor`. Routes tool calls to plugin processes as a fallback layer behind the primary tool registry.

### Key Types (skills.rs)

- **`SkillMetadata`** — Parsed frontmatter: name, hooks, agent, allowed_tools, description, enabled.
- **`Skill`** — Loaded skill with metadata, source, path, and **plugin_name** (for plugin-owned skills).
- **`SkillSource`** — Enum: Bundled, UserDir, RepoDir, Plugin, Mcp. Precedence ordering for conflict resolution.
- **`SkillFile`** — Parsed content: metadata + script body.
- **`SkillLoader`** — Loads `.md` skill files from configured directories with conflict resolution. Plugin and MCP skills can be added programmatically.

### Architecture Patterns

- **DashMap** for concurrent plugin/marketplace registries and active process tracking.
- **AtomicBool** for refresh flag.
- **YAML frontmatter parsing** via `serde_yaml` with custom `deserialize_string_or_list` for scalar/array fields.
- **Layered skill loading**: bundled -> user -> repo (`.coderex/skills`) with precedence-based conflict resolution.
- **Atomic install staging**: temp directory + validation + atomic rename + cleanup on failure.
- **Install locks**: `DashMap<String, Arc<tokio::sync::Mutex<()>>>` prevents concurrent installs.
- **Version compatibility**: SemVer comparison of `min_version` against app version on load and install.
- **Signature verification**: Ed25519 signature check on plugin archives (optional, configured via public key).
- **Dependency resolution**: Kahn's topological sort with cycle detection for plugin load ordering.
- **Error recovery**: Exponential-backoff retry for Error-state plugins (re-reads manifest, re-validates).
- **MCP skills bridge**: Discovered MCP tools converted to `SkillMetadata` and stored in `SessionContext.mcp_skills`.
- **Hybrid execution model**: Default tier = external process (JSON-RPC over stdio). Fast tier = WASM (reserved for future high-performance plugins).
- **Plugin command registration**: Dynamic `SlashCommand` registration via `Box::leak` for `&'static str` names/descriptions.
- **Plugin tool fallback**: `PluginToolExecutor` chains as a tertiary fallback behind primary registry -> MCP -> plugin.

### Public API Surface

- `PluginManager`, `PluginHandle`, `PluginState`, `PluginManifest`, `RuntimeType`, `MarketplacePlugin`, `MarketplaceConnection`, `MarketplaceError`, `ReconciliationReport`, `MarketplaceReconciler`.
- `PluginProcess`, `PluginCommandInfo`, `PluginSkillInfo`.
- `PluginCommandBridge`.
- `PluginToolExecutor`.
- `SkillLoader`, `Skill`, `SkillMetadata`, `SkillSource`, `SkillFile`, `parse_skill_file`.

## Plugin Execution Model

Plugins declare their runtime in `plugin.json` via the `runtime` field (`"process"` or `"wasm"`, default: `"process"`).

### Process Plugins (Default Tier)

External process plugins communicate via JSON-RPC 2.0 over stdio using the `coderex/` method namespace:

- `coderex/initialize` — Protocol version and capabilities exchange.
- `coderex/commands/list` — Discover commands.
- `coderex/commands/execute` — Execute a command by name with args.
- `coderex/skills/list` — Discover skills.
- `coderex/skills/execute` — Execute a skill by name with context.
- `coderex/ping` — Health check.

Process plugins declare an optional `entrypoint` (e.g., `"bin/plugin"`, `"node index.js"`) and custom `env` variables in their manifest. The orchestrator spawns them during `InitPlugins`, discovers commands/skills dynamically, and wires them into the REPL via `with_plugin_manager`.

### WASM Plugins (Fast Tier — Future)

WASM runtime is gated behind the `wasm-runtime` Cargo feature (`wasmtime` dependency). Currently, `spawn_plugin` logs a warning and skips WASM plugins. The infrastructure (manifest field, feature flag) is in place; the `wasmtime` integration is reserved for future high-performance, sandboxed plugins.

## Skill Execution Model

- **Plugin skills** (`SkillSource::Plugin`): Forwarded to their owning plugin process via `PluginManager::execute_skill`.
- **Non-plugin skills** (bundled, user, repo, MCP): Treated as **prompt templates** — their script path is returned for rendering into the system prompt or message context.

## Orchestrator Integration

During startup (`phase_init_plugins`):
1. Discover plugins from `.coderex/plugins/` and config home `plugins/`.
2. Load each `plugin.json` manifest into `PluginManager`.
3. For process plugins: spawn, initialize, discover commands/skills dynamically.
4. Store `PluginManager` in `SessionContext`.

During REPL construction:
1. Call `ReplLoop::with_plugin_manager(pm)` to register plugin slash commands.
2. Layer `PluginToolFallbackExecutor` behind the primary `RegistryToolDispatcher` so plugin tools act as a fallback.

## Current Status

### What Works

- Plugin discovery from directories (reads `plugin.json`).
- Plugin load/unload with state tracking and version compatibility checks.
- **Process plugin spawning** with JSON-RPC stdio transport, initialize handshake, and dynamic command/skill discovery.
- **Plugin command registration** as namespaced slash commands (`/{plugin}/{command}`).
- **Plugin tool execution** via `PluginToolExecutor` implementing `ToolExecutor`, chained as fallback.
- **Plugin skill execution** forwarded to owning plugin process; non-plugin skills returned as prompt template paths.
- Marketplace registration with HTTPS enforcement and optional domain allowlist.
- Configurable marketplace endpoint paths (`list_endpoint`, `download_endpoint`) with defaults.
- Plugin installation: download -> temp staging -> archive extraction (tar+gzip) -> validation -> atomic rename -> cleanup.
- Archive extraction: `tar` + `flate2` unpacks `plugin.tar.gz` and validates `plugin.json` presence.
- Rollback on failed install: temp directory is deleted if any validation step fails.
- Concurrent install protection: per-plugin-name install locks via `DashMap<String, Arc<Mutex<()>>>`.
- Unload cleanup: removes plugin directory from disk.
- Version compatibility: `min_version` compared against app version using `semver` crate.
- Signature verification: Ed25519 signature check (optional, configured via `set_public_key`).
- Dependency resolution: topological sort with cycle detection via Kahn's algorithm.
- Error recovery: `retry_failed_plugins()` attempts to re-read manifests for Error-state plugins up to `max_retries`.
- Reconciliation: detect new, updated, removed plugins.
- Auto-install new plugins from marketplace.
- Skill loading from all 5 sources with conflict resolution.
- Skill frontmatter parsing via `serde_yaml` with custom scalar/array deserializers.
- Repo-specific skills from `.coderex/skills/`.
- MCP skills bridge: `phase_init_mcp` converts discovered MCP tools to `SkillMetadata`.
- **WASM feature flag prep**: `process-runtime` (default) and `wasm-runtime` Cargo features.

### Tests

- 30+ tests in `plugins.rs` (manager ops, load/unload, marketplace, discovery, reconciler, URL validation, version checks, signature verification, dependency resolution, error recovery, custom endpoints).
- 16+ tests in `skills.rs` (parsing, loader, plugin/MCP skills, repo dir, conflict resolution, precedence, quoted strings).
- 6 tests in `runtime.rs` (spawn, initialize, terminate, invalid command).
- Tests use `tempfile` for filesystem operations.
