# plugins Module Inspection

## Goal

Manages plugin lifecycle (discovery, loading, execution, unloading), marketplace connectivity (multiple marketplaces), and skill loading from multiple sources (bundled, user dir, repo dir, plugin, MCP).

## Design

### Key Types (plugins.rs)

- **`PluginState`** — Lifecycle enum: Discovered, Loading, Active, Error(String), Uninstalled, Installing.
- **`PluginHandle`** — Runtime representation with name, version, state, path, commands, skills, retry_count, max_retries, **verified**.
- **`PluginManifest`** — Deserialized from `plugin.json`. Identity/metadata (`name`, `displayName`, `version`, `description`, `author`, `homepage`, `repository`, `license`, `keywords`, `defaultEnabled`, `metadata`, `$schema`), CodeRex execution fields (`min_version`, `commands`, `skills`, `dependencies`, `signature`, `runtime`, `entrypoint`, `env`), `userConfig`, and the Claude component fields CodeRex acts on. See the field table below.
- **`PluginAuthor`** — `{name, email?}`; parses from either an object (Claude's shape) or a bare string.
- **`UserConfigField`** — One `userConfig` entry: `type`, `title`, `description`, `required`, `default`, `sensitive`, `multiple`, plus `options`/`min`/`max`, all enforced against a supplied value at resolution time.
- **`PluginSource`** — Install source: `Marketplace { name }`, `LocalDir(path)`, `Git { url, rev }`, `Github { repo, rev, sha }`, `GitSubdir { url, path, rev, sha }`, `Archive { url, sha256 }`, and `Unsupported { kind }`; `PluginSource::detect()` classifies a CLI argument.
- **`PluginPin` / `PluginPinStore`** — Trust-on-first-use record of what was installed (archive SHA-256, git commit), read from `.coderex-pins.json` in the install directory.
- **`RuntimeType`** — Enum: `Process` (default), `Wasm`. Declares how the plugin executes.
- **`MarketplacePlugin`** — Marketplace listing with download URL.
- **`MarketplaceConnection`** — HTTP client + base URL + configurable endpoint paths per marketplace.
- **`MarketplaceError`** — URL validation errors: InsecureUrl, InvalidUrl, NotAllowed.
- **`SignatureCheck`** — Three-state result of a signature check: `Verified`, `Unverified` (no key configured). An error means a configured key did not match.
- **`PluginManager`** — `DashMap`-backed registry of plugins and marketplaces with install locks, version checks, signature verification, dependency resolution, **active process tracking**, and host-supplied `userConfig` values.
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
- **Signature verification**: Ed25519 check of the downloaded archive against a **detached** signature fetched from `<archive-url>.sig` (hex), optional and configured via `CODEREX_PLUGIN_PUBLIC_KEY`. The signature cannot live inside the archive it signs. With a key configured, sources that cannot carry a detached signature (local directory, git, git-subdir) fail closed unless an existing TOFU pin matches; without a key they install as `Unverified` with a warning.
- **Manifest strictness**: `deny_unknown_fields` on `PluginManifest` and `UserConfigField`, plus a warning for accepted-but-unimplemented fields.
- **Environment injection**: `userConfig` values and static `env` are resolved and substituted before the child is spawned.
- **Trust on first use**: archive SHA-256 / git commit recorded in `.coderex-pins.json` and checked on later installs.
- **Dependency resolution**: Kahn's topological sort with cycle detection for plugin load ordering.
- **Error recovery**: Exponential-backoff retry for Error-state plugins (re-reads manifest, re-validates). A successful retry reports `RestartRequired` (or invokes a registered restart handler); the CLI acts on that signal by calling `spawn_plugin` again, so recovery restores a runtime instead of only clearing the error state.
- **MCP skills bridge**: Discovered MCP tools converted to `SkillMetadata` and stored in `SessionContext.mcp_skills`.
- **Hybrid execution model**: Default tier = external process (JSON-RPC over stdio). Fast tier = WASM (compiled in-process, sandboxed, and loaded by `spawn_plugin`).
- **Plugin command registration**: Dynamic `SlashCommand` registration via `Box::leak` for `&'static str` names/descriptions.
- **Plugin tool fallback**: `PluginToolExecutor` chains as a tertiary fallback behind primary registry -> MCP -> plugin.

### Public API Surface

- `PluginManager`, `PluginHandle`, `PluginState`, `PluginManifest`, `PluginAuthor`, `UserConfigField`, `RuntimeType`, `MarketplacePlugin`, `MarketplaceConnection`, `MarketplaceError`, `ReconciliationReport`, `MarketplaceReconciler`.
- `PluginSource`, `PluginPin`, `PluginPinStore`, `PIN_FILE_NAME`, `sha256_hex`, `substitute_user_config`, `PLUGIN_OPTION_ENV_PREFIX`, `PLUGIN_PUBLIC_KEY_ENV`, `SignatureCheck`, `unsupported_source_reason`.
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

### WASM Plugins (Fast Tier)

The WASM runtime is enabled by default: `crates/plugins/Cargo.toml` has
`default = ["process-runtime", "wasm-runtime"]`, and `wasm-runtime` pulls in
`wasmtime` 20. With the feature off, `PluginWasmInstance::new` returns a
"WASM runtime not enabled" error instead of loading anything.

`spawn_plugin` loads a `runtime: "wasm"` plugin from its `entrypoint` (default
`plugin.wasm`) by compiling the module, running `coderex_initialize`, and
discovering commands and skills, then keeps the instance in
`active_wasm_instances`. `userConfig` is resolved by the same code path as for
a process plugin and delivered through the host import described below. A WASM
manifest's `entrypoint` is a file name, so `${user_config.KEY}` substitution
applies to process `env`/`entrypoint` only; the resolved values reach a WASM
module through the import.

The sandbox is enforced by the host, not by trusting the module:

- **Fuel**: every call gets a fresh store with 100,000,000 fuel units
  (`PLUGIN_FUEL`). Fuel is charged per WebAssembly instruction, so a runaway
  loop traps instead of pinning the thread.
- **Memory**: a `StoreLimits` resource limiter caps guest linear memory at
  64 MiB (`PLUGIN_MAX_MEMORY_BYTES`). It is consulted both for memory declared
  at instantiation and for every `memory.grow`.
- **Shared memory**: the threads proposal is disabled (`wasm_threads(false)`),
  so a module cannot create a shared memory (which wasmtime's store limiter
  deliberately does not bound) or use atomics.
- **Minimal host interface**: the only import the host provides is
  `coderex.user_config`. WASI and everything else remain unlinked, so a module
  importing them fails to instantiate. A module that imports nothing is still
  instantiated with an empty import set and runs unchanged.

#### `userConfig` ABI for WASM plugins

The host links one function:

```wat
(import "coderex" "user_config"
  (func $user_config (param i32 i32) (result i32)))
;; (ptr, capacity) -> required_len
```

It writes the resolved options into the plugin's exported linear `memory` at
`ptr` and returns the **full** JSON length, not the number of bytes written.
The JSON value is one object whose keys and values are exactly what a process
plugin receives as `CODEREX_PLUGIN_OPTION_<KEY>` (and what `${user_config.KEY}`
substitutes). For example:

```json
{"REGION":"eu","TOKEN":"s3cret"}
```

- An option with no value is **absent** from the object. It is not `null` and
  not an empty string, matching the process path, where no environment
  variable is set.
- Sensitive options are included verbatim. As with environment injection, the
  host is expected to have resolved any `keyring://` reference first; an
  unresolved one stops the spawn. The runtime does not redact the value from
  the plugin it is being given to.
- The guest can call `(0, 0)` to query the length without writing anything,
  then allocate a buffer and call again. If the returned length is greater
  than the capacity passed, the JSON was truncated (only `capacity` bytes are
  written), so query first or retry with a larger buffer.
- No NUL terminator is written: the returned length is authoritative. (Do not
  assume the byte after the JSON belongs to the host.)
- The host returns `-1` for a negative pointer/capacity, a missing `memory`
  export, or a write that would fall outside guest memory.
- Options apply to the whole instance: every call gets a fresh store, but the
  same serialized options are served each time.

**Existing plugins must import the function to receive options.** Linking is
opt-in at the module level: a plugin that does not declare the import
instantiates exactly as it did before and simply has no way to read the
options. This keeps the empty-import set working, at the cost that adding
`userConfig` to a plugin requires an ABI change on the plugin side (a new
import, and a rebuild) rather than being transparent.

Compilation is bounded in two layers. The first runs in-process and bounds the
*work*, the second runs in a killable child and bounds the *time*. Compilation
is mandatory out-of-process: there is no in-process fallback, because wasmtime
20 has no compile-time interruption.

**Layer 1 — linear pre-scan (always on).** Before any compiler process is
started, `PluginWasmInstance::new` parses the module linearly with `wasmparser`
and rejects it when any of these limits is exceeded:

| Limit | Constant | Value |
|---|---|---|
| Module file | `PLUGIN_MAX_WASM_BYTES` | 16 MiB |
| Code section bytes | `PLUGIN_MAX_CODE_SECTION_BYTES` | 8 MiB |
| Declared functions | `PLUGIN_MAX_FUNCTIONS` | 50,000 |
| Total operators (all bodies) | `PLUGIN_MAX_OPERATORS` | 1,000,000 |
| `block`/`loop`/`if` nesting | `PLUGIN_MAX_NESTING_DEPTH` | 256 |
| WAT text input | `PLUGIN_MAX_WAT_BYTES` | 1 MiB |

The file-size check runs from metadata before reading and again after reading,
so an oversized file is never loaded. The pre-scan translates WAT text to the
binary format first, so the text form cannot bypass the limits; every error
names the limit it crossed. The scan deliberately does not validate the module
— wasmtime still does that — it only measures work. The compiler process repeats
the pre-scan on the bytes it receives.

**Layer 2 — host-injected out-of-process compilation.** The runtime takes a
compiler command from its host: a program plus leading arguments
(`WasmCompilerCommand` / `set_compiler_command`). The CLI sets it to
`std::env::current_exe() --internal-wasm-compile`, and handles that hidden
argument *before* clap parsing. The runtime appends `<input> <output>` and runs
the result as a child. Because the host re-executes itself, `cargo run`,
packaged builds with only the `coderex` binary, and the separate
`coderex-wasm-compile` helper all use the same path.

The helper binary is **deliberately kept**, not a leftover. The production CLI
path no longer needs it, but `crates/plugins/tests/wasm_compile_subprocess.rs`
drives a real compiler subprocess through `CARGO_BIN_EXE_coderex-wasm-compile`,
and `coderex-cli` depends on `coderex-plugins` — so the plugin crate cannot
dev-depend on the CLI to exercise its own `--internal-wasm-compile` path.
Removing the `[[bin]]` would mean deleting the only test that runs the compiler
as a separate process, so it stays and is built by default.

**Fail closed.** When no command is configured *and* no helper is found next to
the executable, the load is refused with an actionable error naming
`set_compiler_command`. A compiler that exits non-zero, times out, or emits
bytes this runtime cannot deserialize is also an error. The runtime never
falls back to compiling untrusted code in-process.

**Killing the tree.** The child is spawned in its own process group
(`process_group(0)`), and a timeout sends `SIGKILL` to the whole group before
reaping the direct child. On Windows the compiler runs under the shared
`SandboxHandle` (the same one the shell tools use): a suspended child, an
AppContainer token attached after spawn, and an anonymous Job Object with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. A timeout calls `TerminateJobObject` via
`SandboxHandle::terminate`, and closing the handle kills anything left. No
helper descendant survives a timeout.

The parent also installs:

- `RLIMIT_CPU` = 20 s (Unix, mandatory);
- a 2 GiB memory limit (Unix, best-effort): `RLIMIT_AS` on Linux and other
  Unix, `RLIMIT_DATA` on Darwin (see the boundary note);
- a 30 s wall-clock timer;
- on Windows, Job Object job user-time and job memory limits in addition to
  kill-on-close.

**OS sandbox around the compiler.** Where the core sandbox primitives fit, they
wrap the compiler as well as the process-group/job kill:

| Platform | Applied to the compiler process | What it confines |
|---|---|---|
| macOS | `sandbox-exec(1)` profile from `coderex_core::sandbox` | reads to system roots plus the staging directory and compiler binary; writes to the staging directory only; network denied |
| Linux with `bwrap` | bubblewrap mount/network namespace | read-only root, staging directory the only writable path, network namespace unshared |
| Windows | AppContainer + Job Object (`SandboxHandle`) | reads to system roots, the staging directory and the compiler directory; writes to the staging directory; execute of the compiler; network denied; CPU/memory and kill-on-close |

The Linux seccomp allowlist is deliberately not applied: it is written for
shells and would reject wasmtime's syscalls. Bubblewrap is skipped when the
staging directory is inside the system temp directory, because `bwrap` replaces
`/tmp` with a fresh tmpfs and the bind mount would resolve against it; in that
layout the compiler gets rlimits and the process-group kill only. On macOS, if
`sandbox-exec` is missing or profile creation fails, the compiler likewise runs
with rlimits and the process-group kill only. Those degradations are logged;
this document does not claim a sandbox that was not applied.

**AOT cache.** A module is compiled under the helper on first load, and
`Module::serialize` output is stored for later loads:

- **Location**: `CODEREX_WASM_CACHE_DIR` if set, otherwise
  `~/Library/Caches/coderex/wasm` (macOS), `$XDG_CACHE_HOME/coderex/wasm` or
  `~/.cache/coderex/wasm` (Linux), `%LOCALAPPDATA%\coderex\wasm` (Windows).
  Hosts can also call `set_cache_dir`.
- **Key**: `<engine-config-hash>/<module-sha256>.cwasm`. The engine hash is
  SHA-256 over wasmtime's `precompile_compatibility_hash` (version plus every
  code-generation-relevant config knob) and a format tag; the module hash is
  SHA-256 of the exact pre-scanned bytes (WAT is translated first).
- **Verification**: entries carry a header with both hashes and the payload
  length. Before `Module::deserialize`, both are compared against values
  recomputed for this load; a mismatch or a deserialize failure deletes the
  entry and recompiles. wasmtime independently rejects precompiled code built
  by an incompatible engine.
- **Write**: atomically (temp file in the engine directory, then rename), with
  a lost race treated as success when a complete entry is already present.
- **Pruning**: after each write the cache is pruned oldest-first to
  `WASM_CACHE_MAX_BYTES` (256 MiB). The newest entry is kept even if it alone
  exceeds the bound. Staging directories for in-flight compilations are not
  counted or pruned.

**Honest remaining boundaries:**

- On Darwin the memory bound is `RLIMIT_DATA`, because Darwin rejects finite
  `RLIMIT_AS` values with `EINVAL`. `RLIMIT_DATA` caps the data segment rather
  than the whole address space: direct mappings, the stack and the executable
  image are outside it, so it is **not a full address-space cap**. The limit is
  best-effort (a failure is ignored, because it cannot be logged from
  `pre_exec`); the child still has the hard CPU limit, the wall-clock kill and
  the sandbox-exec profile. Other Unix platforms use `RLIMIT_AS`, subject to
  the same best-effort handling.
- The Windows compiler sandbox uses `coderex_core::sandbox`; its Win32 calls are
  exercised by the `test_windows` CI job (`wasm_compile_subprocess`), not by a
  macOS host.
- The compiler child inherits this process's environment. It is the same
  binary/trust domain as the host, so credentials are not scrubbed from it (in
  contrast to third-party tool processes).
- The AOT cache trusts the user's private cache directory. The envelope checks
  integrity and engine/module correspondence, not authenticity; a same-user
  process able to write there is already inside the trust boundary.
- macOS `sandbox-exec` is deprecated by Apple. The runtime probes for it and
  falls back to rlimits + process-group kill when it is unavailable.
- The helper path, timeout, and test hooks have debug-build-only environment
  overrides (`CODEREX_WASM_COMPILE_HELPER`, `CODEREX_WASM_COMPILE_TIMEOUT_MS`,
  and the `CODEREX_WASM_COMPILE_TEST_*` variables) used by the tests. Release
  builds neither read nor compile the test hooks.
- wasmtime 20 has no compile-time fuel or epoch option;
  `parallel_compilation(false)` only chooses single-threaded compilation (it
  does not reduce total work), and `max_wasm_stack` bounds the guest stack at
  runtime, not compilation.

## `plugin.json` Field Table

The manifest is parsed with `deny_unknown_fields`: a field that is not in the table below is a hard parse error naming the field, not something silently dropped. That was the real defect — an author could write `displayName` and be told nothing, because the whole field vanished at parse time. `discover()` likewise logs a warning for a manifest it cannot parse instead of skipping it in silence.

| Field | Status | Notes |
|---|---|---|
| `$schema`, `name`, `displayName`, `version`, `description`, `homepage`, `repository`, `license`, `keywords`, `defaultEnabled`, `metadata` | ✅ parsed | Identity and metadata. `author` accepts a string or `{name, email}`; an object used to fail the whole manifest. |
| `min_version`, `commands`, `skills`, `dependencies`, `signature`, `runtime`, `entrypoint`, `env` | ✅ acted on | CodeRex execution fields (pre-existing). |
| `userConfig` | ✅ implemented | Values are validated against the declared schema (`type`, `options`, `min`, `max`, `required`, `default`) before they reach a plugin. See "userConfig injection". |
| `hooks` | ✅ implemented | Active plugins contribute their hooks to the `Plugin` tier. The value may be a bare array or `{"hooks":[...]}`; `PluginManager::plugin_hooks()` merges them in plugin-name order and logs malformed values. |
| `agents` | ✅ implemented | Active plugins contribute definitions through `PluginManager::plugin_agents()`; the REPL registers them as custom agents. |
| `outputStyles` | ✅ implemented | Name-keyed registry; `/style <name>` applies one as extra system instructions. |
| `themes` | ✅ implemented | Name-keyed registry; `ThemeManager::register_json` makes them selectable via `/theme`. |
| `workflows` | ✅ implemented | Named scripts registered with the Workflow tool. |
| `monitors` | ✅ implemented | Persistent watch scripts armed with the session (`PluginMonitorSupervisor`). |
| `mcpServers` | ✅ implemented | Active plugins contribute `{"mcpServers": {...}}` through `PluginManager::plugin_mcp_servers()`; the orchestrator loads them before `.mcp.json` (project file wins on name clashes). |
| `lspServers` | ✅ implemented | The first configured server is the default for the `LSP` tool (the REPL fills the tool's shared default at startup). |
| `channels` | ✅ implemented | A channel names a configured MCP server and routes that server's JSON-RPC notifications to subscribed sinks (`PluginManager::subscribe_channel` / `route_channel_notification`). `/plugin components` lists each channel with its live state — `subscribed`, `idle (no subscriber)`, or `server not configured`. See "Channels". |
| `binaries` | ✅ implemented | SHA-256-pinned HTTPS fetch into `<plugin>/bin/` (`fetch_plugin_binaries`); `CODEREX_PLUGIN_NO_BINARIES=1` skips the fetch. |
| `types` | ✅ implemented | Validated `.d.ts` files are exported at startup to `config_home()/plugin-types/<plugin>/` (`export_plugin_types`) so a plugin author has a stable include path, **and** read into a `PluginTypeRegistry` that `/plugin components` lists. The same export writes the host's generated surface — `coderex/`, `coderex-tools/`, `coderex-mcp/` and a `tsconfig.json` — from the registered tool schemas; `/plugin types [name]` lists or prints them. CodeRex has no TypeScript runtime, so "loaded" means read into memory and exposed — never executed. See "Types". |
| `settings` | ✅ implemented (allowlisted) | A typed patch deep-merges into the resolved settings, by source: user/project values win over every plugin, and a plugin only fills a key the user left unset. `permission_mode`, `telemetry` and `max_budget_usd` are tighten-only; `hooks`, `mcpServers`, `api_key` and `env` are never taken. Unknown keys warn and are ignored. See "Settings". |
| `experimental` | ✅ implemented (raw) | Exposed per plugin; expansion (themes/monitors/outputStyles/…) is the caller's job. |

All declared component fields now have a runtime surface. `PluginManifest::unimplemented_fields()` is empty and remains as the guard against a future field silently doing nothing.

## `userConfig` Injection

A manifest declares options, and `spawn_plugin` turns them into the child's environment before the process is spawned:

```json
{
  "name": "my-plugin",
  "entrypoint": "bin/plugin --region ${user_config.REGION}",
  "env": { "ENDPOINT": "https://${user_config.REGION}.example.com" },
  "userConfig": {
    "TOKEN":  { "type": "string", "title": "API token", "sensitive": true, "required": true },
    "REGION": { "type": "string", "default": "eu" }
  }
}
```

- Each resolved option becomes `CODEREX_PLUGIN_OPTION_<KEY>` in the plugin process (Claude's `CLAUDE_PLUGIN_OPTION_<KEY>`).
- `${user_config.KEY}` is substituted in the manifest's `env` values and in the `entrypoint`. An unresolved placeholder is left as written and logged, so an author can see what did not resolve.
- Values are resolved in order: what the host supplied via `PluginManager::set_plugin_option`, then CodeRex's own `CODEREX_PLUGIN_OPTION_<KEY>`, then the manifest's `default`.
- A supplied value is checked against the declared schema before it is injected (`resolve_options` → `validate_option_value`):
  - `options` (an array, or `{ "value": ..., "label": ... }` objects) is an allowlist; a value outside it is refused.
  - `type: "number"` (or a declared `min`/`max`) requires a number, and `min`/`max` are enforced inclusively.
  - `type: "boolean"` requires exactly `true` or `false`.
  - Every rejection is an error naming the option, the value, and what was expected — the spawn stops rather than handing the plugin a value its own manifest rejects.
- Option keys must be usable as environment variable names (`[A-Za-z_][A-Za-z0-9_]*`); anything else is refused, because a key with a dash or an `=` in it is a way to smuggle a second variable into the environment.
- WASM plugins use the same resolution and the same validation errors, but the resolved values are encoded as one JSON object and served through the `coderex.user_config` import rather than an environment (see "`userConfig` ABI for WASM plugins"). A WASM plugin receives them only if it imports that function; a required option with no value still stops the spawn, even though there is no environment.

**Sensitive options.** There is no keychain dependency in this crate (the workspace's keychain access lives in `coderex-config`, behind its `keyring` feature), so the sensitive path is strict rather than quiet:

- A `sensitive` option may **not** carry a plaintext `default` — a secret in `plugin.json` is a secret in a file that gets committed. That is a load-time error.
- A sensitive value that is still an unresolved `keyring://item` reference is refused: injecting the literal reference would hand the plugin a string that is not the secret, and resolving it needs the keychain. The host is expected to resolve it first (`coderex_config::resolve_api_key` is the existing convention).
- A `required` option with no value stops the spawn with an error naming the variable to set. A plugin that silently runs without the secret it asked for is worse than one that does not run.
- An optional option with no value is simply not injected (debug log).
- Sensitive values travel by environment variable (process plugins) or through the resolved options object (WASM plugins). Either way they are visible to the plugin that asked for them; for process plugins they are also visible in the process table of the same user, which is Claude's design too.

**Honest gap:** there is no interactive prompt and no plugin settings store, so options come from the host or from the environment — nothing writes a user's answers back to `settings.json` the way Claude does. Values are validated against the declared schema (above), but there is still no prompt to *collect* a missing one. For WASM plugins, options require an explicit import in the module; they are not delivered retroactively to existing plugins.

## `settings`

A plugin's `settings` is a narrow allowlist, and it is a *patch*, not a replacement. The patch deep-merges into the session's resolved settings **by source**: user and project settings outrank every plugin, so a plugin can supply a default the user never set but can never override one they did.

| Group | Keys | Rule |
|---|---|---|
| Mergeable | `model`, `context_window`, `vim_mode`, `statusline` | Fill only a key the user left unset; the user's value wins, recursively for nested objects. |
| Tighten-only | `permission_mode`, `telemetry_enabled`, `telemetry`, `max_budget_usd` | May only make the session stricter. `permission_mode` moves only toward the more restrictive end (`plan` < `dont-ask` < `default`/`auto` < `accept-edits` < `bypass-permissions`); `telemetry_enabled` may only go to `false`; `max_budget_usd` may only lower the ceiling. A `telemetry` object is refused outright. |
| Never taken | `hooks`, `hook_layers`, `mcpServers`, `mcp_servers`, `api_key`, `env`, `config_version` | Security-sensitive; ignored with a warning, whatever the value. |

An unknown key is warned about and ignored. A refused or loosen-only value is warned about and ignored — never applied.

## Channels

A `channels` declaration names a configured MCP server and gives it a name:

```json
{ "channels": [ { "server": "slack", "displayName": "Team chat" } ] }
```

- `PluginManager::plugin_channel_report()` returns one entry per declared channel with its **live** state: `Subscribed` (a sink is attached), `Idle` (the server is configured but nothing is subscribed), or `ServerNotConfigured`. A channel naming an unconfigured server is reported as such rather than shown as working.
- `subscribe_channel(server, sink)` attaches a `ChannelSink`; `route_channel_notification(server, method, params)` turns one JSON-RPC notification from that server into a `PluginChannelEvent` and hands it to every subscribed sink, returning how many received it. `0` is an honest answer — nothing is bound or nothing is subscribed — not an error.
- `/plugin components` lists each channel as `displayName -> server [state]`.
- The MCP client **does** drive it: `McpManager` routes every notification a configured server emits through `PluginChannelRouter` into `route_channel_notification`, and the session subscribes a production sink to every declared channel.
- **Honest limit:** this is routing plus an in-process fan-out, not a transport. The sink records events into a bounded session log; nothing carries a channel message out of the process (a chat product, another host) and there is no inbound path. `PluginChannelEvent` is deliberately transport-free.

## Plugin UI

A plugin **command** may return a JSON object with a `ui` payload instead of a
plain string:

```json
{
  "text": "optional summary printed before the panel",
  "ui": {
    "title": "Deploy status",
    "style": "info",
    "lines": ["api: ok", "web: failed"]
  }
}
```

`plugin_ui::render_plugin_response` turns it into a titled text panel (a
scrollable message in the TUI, the same box on the terminal). `style` may be
`info` (default), `warning` or `error`; the last two prefix the title with a
marker. Anything that is not a JSON object carrying `ui` is passed through
unchanged, so a plugin that prints JSON still prints JSON.

**Honest limit:** plugins run out-of-process, so this is a structured text
block, not a widget tree. There is no `Box`/`Text`/`Button`, no interaction,
and no per-surface rendering (`terminal`/`desktop`/`vscode`); the reference's
in-process mods get those because they run inside its UI runtime.

## Types

There are two halves to the type surface: the declarations a plugin ships, and the declarations CodeRex generates for its own builtin surface. Both are exported under `config_home()/plugin-types/`.

### The plugin's own declarations

A plugin may export TypeScript declarations:

```json
{ "types": ["./types/index.d.ts"] }
```

- Every declared path is validated (relative, no `..`, ends in `.d.ts`) and exported at startup to `config_home()/plugin-types/<plugin>/` by `PluginManager::export_plugin_types`, so a plugin author has a stable include path. Declared files are flattened to their file name under the plugin directory (`types/index.d.ts` becomes `<plugin>/index.d.ts`).
- They are also read into a `PluginTypeRegistry` (`PluginManager::plugin_type_registry()`), and `/plugin components` lists each file as `name (plugin)`.
- A file that cannot be read is recorded in the registry's error list (and warned about) rather than dropped.

### The host's generated declarations

At startup the same call writes CodeRex's own type surface, generated from the actual registered tool schemas. The root contains:

| Artefact | Contents |
|---|---|
| `coderex/index.d.ts` | `declare module 'coderex'`: `EngineInterface`, `HookContext` (session id, cwd, model, permission mode, turn number), `Hook`, `HookResult`, `HookEvent`, and `PermissionMode`. |
| `coderex-tools/index.d.ts` | `declare module 'coderex-tools'`: `interface BuiltinToolInputs { <ToolName>: <input schema> }` and `interface BuiltinToolResults { <ToolName>: <result schema> }`, one member per builtin tool. |
| `coderex-mcp/index.d.ts` | `declare module 'coderex-mcp'`: `McpServer` with name, transport, `command`/`url`, headers/env, and `status`, plus the `McpTransport` and `McpServerStatus` unions. |
| `tsconfig.json` | `compilerOptions` with `strict` and `noEmit`, and an `include` array listing every emitted directory. A plugin can extend it. |
| `<plugin>/` | Each active plugin's declared `.d.ts` files, as above. |

The generator is `crates/plugins/src/declarations.rs`; the four pure entry points are `builtin_tools_declaration` / `builtin_tools_declaration_with_results`, `engine_declaration`, `mcp_declaration`, and `tsconfig_declaration`. `PluginManager::export_plugin_types` is the IO half: it writes the three generated modules, the plugin-declared files, and the `tsconfig.json` whose `include` lists the directories it actually emitted.

The CLI supplies the tool schemas from the builtin tool registry (`register_builtin_tools` → `all_descriptors`). A tool's `input_schema` becomes its input type; its `output_schema` becomes its result type when one is declared, and `unknown` otherwise. Tool names are sorted, so the file is stable from run to run. MCP and other dynamic tools are not in the generated file: they are only known after a session connects.

### JSON Schema → TypeScript

The conversion is intentionally bounded. It handles the shapes the builtin schemas use and falls back to `unknown` rather than inventing a type:

| JSON Schema | TypeScript |
|---|---|
| `type: "object"` + `properties` + `required` | `{ name: T; optional?: T }`; members not listed in `required` are optional. |
| `type: "string"` / `"number"` / `"integer"` / `"boolean"` / `"null"` | `string` / `number` / `number` / `boolean` / `null`. |
| `type: "array"` + `items` | `Array<T>`; `items` absent → `Array<unknown>`. |
| `enum` | A union of literals (`"a" \| "b"`), `never` when empty. |
| `const` | The literal value. |
| `anyOf` / `oneOf` | A union of the converted branches, with duplicates dropped. |
| `additionalProperties` | An index signature; an explicit property union is widened to keep the emitted signature valid. |
| Anything else (`$ref`, unknown `type`, no `type`) | `unknown`. |

Property and tool names that are not TypeScript identifiers are quoted (`"run-in-background"?: boolean`). Nested objects are indented from their enclosing context.

### Reading the generated declarations

- `/plugin types` lists every emitted declaration with its path: `coderex`, `coderex-tools`, `coderex-mcp`, `tsconfig`, and one entry per plugin directory.
- `/plugin types <name>` prints one. The name is the listed name; a relative path such as `coderex-tools/index.d.ts` also works. A name that is not in the list is an error — the resolver only matches already-listed files, so it cannot read outside the type root.
- `/plugin components` continues to list the *plugin-declared* declarations as `name (plugin)`; the generated modules are a separate artefact.

### Deliberately out of scope

- **No TypeScript execution.** The Rust binary does not run a TypeScript compiler, type-check the generated output, or host a language server. "Generated" and "loaded" both mean text that is written, read into memory, and printed; it is never evaluated.
- **No schema compilation for validation.** TypeScript types are for plugin authors' editors and builds; CodeRex's own runtime validation is unchanged.
- **No invented types.** A schema feature the converter does not understand is `unknown`, not a guess. In particular `$ref` is not resolved across documents.

## Install Sources

`PluginSource` has seven kinds, and `PluginSource::detect()` classifies a CLI argument into one of them (a `github:`/`git-subdir:` prefix → that kind; an existing directory → `LocalDir`; a git remote → `Git`; an HTTPS `.tar.gz`/`.tgz` → `Archive`; anything else → a marketplace listing name, which is what `coderex plugin install <name>` has always meant). `install_plugin` routes a listing whose url is not a marketplace URL to `install_from_source`, so `coderex plugin install ./my-plugin` and `coderex plugin install https://github.com/acme/plugin.git` work without the CLI changing its argument shape.

| Kind | Behaviour |
|---|---|
| `Marketplace { name }` | The original path: HTTPS listing → download endpoint → extract → validate → atomic rename. |
| `LocalDir(path)` | Reads `plugin.json`, copies the directory into a staging directory, then renames it into place. The source is copied, not moved or linked, so it stays a working copy. |
| `Git { url, rev }` | Shallow clone (`--depth 1`, `--branch <rev>`); a full commit SHA as `rev` clones the history and checks it out. `.git` is dropped afterwards so the install is a snapshot, not a nested repository. The resolved commit is the plugin's pin. |
| `Github { repo, rev, sha }` | `owner/repo` is expanded to `https://github.com/<owner>/<repo>.git` and installed by the git path, so transport validation, `GIT_TERMINAL_PROMPT=0` and commit pinning all apply unchanged. `repo` must be exactly `owner/repo` — anything with extra segments, a leading `/`, or a `..` segment is refused before a URL is built. |
| `GitSubdir { url, path, rev, sha }` | Clone, then install the named subdirectory as the plugin. `path` is resolved *inside* the clone: an absolute path or a `..` component is refused syntactically, and the joined path is then canonicalised and must still be under the clone root, so a committed symlink pointing outside the checkout is refused too (a committed symlink is the case the syntactic check alone misses). The clone's resolved commit is the pin, exactly as a plain git install. |
| `Archive { url, sha256 }` | HTTPS only. Signature-checked like a marketplace archive, and the archive's SHA-256 is recorded as a pin. |
| `Unsupported { kind }` | A source the reference defines but CodeRex does not implement. Recognised so the refusal is explicit (see "npm and command" below). |

Git transports that run commands are refused before `git` is spawned: `ext::` and any other `<name>::` transport is rejected, and only `https://`, `ssh://`, `git://`, `file://` and `user@host:path` are accepted. Credential prompts are disabled (`GIT_TERMINAL_PROMPT=0`) so an unauthenticated clone fails with a message instead of hanging, and `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` are not inherited from the caller.

A `rev` and a `sha` are both the same knob to `git`, but they are not interchangeable to a caller: `sha` must be a full 40-character commit (anything else is refused, so a caller who asked for a commit never silently gets a branch tip of the same name), and when both are present the `sha` wins. Which one a spec string carries is decided by content — a 40-hex revision is a `sha`, anything else is a `rev`.

### Source spec strings

The reference's source union is structured JSON, but CodeRex's CLI takes one string. The two kinds a shape cannot express by inspection get a `scheme:` prefix; both accept an optional `@<rev>` suffix:

| Spec string | Detected as |
|---|---|
| `github:owner/repo` | `Github { repo: "owner/repo", .. }` |
| `github:owner/repo@main` | `Github { .., rev: Some("main") }` |
| `github:owner/repo@<40-hex>` | `Github { .., sha: Some(..) }` |
| `git-subdir:<clone-url>#<path>` | `GitSubdir { url, path, .. }` |
| `git-subdir:<clone-url>#<path>@v1` | `GitSubdir { .., rev: Some("v1") }` |
| `npm:<package>` | `Unsupported { kind: "npm" }` |
| `command:<command>` | `Unsupported { kind: "command" }` |

For `git-subdir` the split is on the *first* `#` and then the *last* `@` after it, so a scp-like clone URL (`git@github.com:acme/mono.git#plugins/foo`) keeps its own `@` as part of the owner. A path containing `@` is not expressible; use the structured variant.

### Which of Claude's source kinds do not apply

Claude's marketplace entry union has eight-ish kinds. CodeRex implements five of the shapes and deliberately not the rest:

| Claude kind | CodeRex | Why |
|---|---|---|
| local path | ✅ `LocalDir` | Local-first tool; this is the primary source. |
| `github` | ✅ `Github` | `owner/repo` shorthand plus an optional ref; resolved to a clone URL and handed to the git path. |
| `git-subdir` | ✅ `GitSubdir` | A clone narrowed to a named subdirectory, with the git path's transport validation and commit pinning carried over. |
| `url` | ✅ via `Git`/`Archive` | Not a separate kind: `{url, ref?, sha?}` is a clone (covered by `Git`) or, when the URL is an archive, a download (covered by `Archive`). `detect()` already picks between them from the URL itself, so adding a third spelling of the same two kinds would only be a way to get the same thing wrong twice. |
| `archive` | ✅ `Archive` | Straightforward and gives the SHA-256 pin a home. |
| `npm` | ⚪ not implemented | Needs a Node toolchain and registry resolution; CodeRex plugins are not npm packages, and there is no npm registry client anywhere in the workspace. The reference is in the same state — its own source throws `Error("NPM marketplace sources not yet implemented")` (`plugin.md:200`) — so this is recorded as a gap, not built. The spec string `npm:` is still recognised and refused with that same message, so it is never mistaken for a plugin by that name. |
| `command` | ⚪ not implemented | **Decided against on purpose.** See below. |
| `settings` | ⚪ not implemented | The reference's `settings` is a *marketplace* that lives inside settings. CodeRex registers marketplaces through `add_marketplace` (a base URL plus endpoint paths); its settings file has no plugin or marketplace table, so there is nothing to read and no local shape for the kind to take. Claude-internal, and local-first CodeRex has no equivalent container. |
| `unsupported` | ⚪ | Claude's placeholder variant; nothing to implement. |

### Why `command` is not implemented

The reference's `command` source runs a command and reads the absolute plugin directory path it prints. The command comes from a marketplace entry — that is, its text is chosen by a third party — and installing a plugin is not a consent step for arbitrary execution.

None of the guards this crate already has apply here:

- Transport validation (`validate_git_url`) governs `git` transports; it says nothing about a shell command.
- `GIT_TERMINAL_PROMPT=0` stops a prompt, not a command.
- `timeout` bounds *how long* a command runs, not *what* it may do — `rm -rf ~` finishes well inside any timeout.
- `mode: ["copy", "link"]` decides how the reported directory is installed, which happens *after* the command has already run.
- `validate_plugin_name` protects the install destination, not the machine.

There is no sandbox in this crate for running an arbitrary command, and the trigger is not necessarily a user typing it: `install_plugin` routes any listing whose `url` is not a marketplace URL to `install_from_source`, so a marketplace listing with `"url": "command:..."` would reach the resolver. `PluginSource::detect()` therefore maps `command:` to `Unsupported { kind: "command" }`, and `install_from_source` refuses it — the spec is recognised, named, and never executed. A local path or a git URL expresses "the plugin is over there" without the execution, so nothing is lost.

The decision is enforced by a test that asserts a `command:` spec does not become an installable source *and* that the command's side effect never happened.

## Pinning (Trust on First Use)

A configured signing key is the strong check, but a plugin installed with no key configured was previously `Unverified` with only a warning. CodeRex now also records what it installed, in `.coderex-pins.json` inside the install directory:

- **Archive installs** record the archive's SHA-256.
- **Git installs** record the resolved commit.
- **Directory installs** record a deterministic content digest of the directory.

On a later install of the same plugin name, the recorded pin must match, or the install is refused. A pin file that exists but cannot be parsed is an error rather than an empty store — silently forgetting the pins would turn the next install into a fresh first use. A pin that cannot be *written* after a successful install is a warning: the plugin is in place and correct, but the protection it would have had is missing.

When a signing key **is** configured, an unverifiable source (directory/git/git-subdir) is refused unless a matching pin already exists — a fresh install of an unsigned directory or git checkout does not proceed unverified. With no key configured, the old warn-and-install-unverified behavior remains.

**Honest gaps:** the pin is trust-on-first-use, not a signature. There is no pin *file* a user can pre-seed to pin before the first install (the pins are created by the first install). A directory digest pins the bytes but is not a signed identity. A `PluginSource::Archive { sha256 }` caller can pin explicitly for that install. The detached signature must be served beside the archive as `<url>.sig`; there is no separate signature registry.

**Why SHA-256 is implemented in-crate:** `sha256_hex` is the published algorithm, checked against the NIST vectors. The workspace has the `sha2` crate, but adding it to `crates/plugins/Cargo.toml` would change `Cargo.lock`, which is shared, so the function is vendored instead. It is integrity-only, never a signature; swap it for `sha2` as soon as this crate may add dependencies.

## Plugin Registration Contract — ⚪ Product-Form Difference

Claude's runtime mods export `register(on, options)` with `($, e, next)` hook callbacks — a JavaScript module loaded into the host's own runtime, sharing its event bus.

CodeRex plugins are a JSON-RPC 2.0 protocol over stdio (`coderex/initialize`, `coderex/commands/list|execute`, `coderex/skills/list|execute`, `coderex/ping`, `coderex/shutdown`), spawned as separate processes. That is a deliberate, language-agnostic design: a plugin can be written in any language that can speak JSON on stdin/stdout, and it runs in its own process rather than in the host's.

Adopting the JS callback shape would mean either embedding a JS runtime (a large dependency, and a whole engine's worth of sandbox surface) or inventing a lookalike API that is not actually Claude's. Neither buys a plugin author anything the JSON-RPC contract does not already give them. Recorded here as a legitimate difference, not a gap: the capability (a plugin that reacts to events and registers commands/skills) exists, in this protocol's form.

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
- Auto-install new plugins from marketplace: opt-in with `CODEREX_PLUGIN_AUTO_INSTALL=1`; off by default (installing third-party code is a user decision). Signature/pin checks still apply.
- Skill loading from all 5 sources with conflict resolution.
- Skill frontmatter parsing via `serde_yaml` with custom scalar/array deserializers.
- Repo-specific skills from `.coderex/skills/`.
- MCP skills bridge: `phase_init_mcp` converts discovered MCP tools to `SkillMetadata`.
- **WASM runtime enabled by default**: `process-runtime` and `wasm-runtime` are both default Cargo features; WASM calls run under fuel (100M per call), a 64 MiB memory limiter, disabled threads, and one optional host import (`coderex.user_config`). Compilation is guarded by a `wasmparser` pre-scan (16 MiB file, 8 MiB code section, 50,000 functions, 1,000,000 operators, 256 nesting levels) and always runs out-of-process through a host-injected compiler command (the CLI re-executes itself with `--internal-wasm-compile`) with `RLIMIT_CPU` and a best-effort 2 GiB memory limit (`RLIMIT_AS`, or `RLIMIT_DATA` on Darwin), a wall-clock timeout, and a process-group or Job Object kill. There is no in-process fallback: a missing or failing compiler refuses the load. Successful compilations are cached as serialized modules keyed by engine-config and module SHA-256 (256 MiB bounded).
- **WASM `userConfig` injection**: the same resolution and validation as process plugins, delivered to a module that imports `coderex.user_config(ptr, capacity) -> required_len` as one JSON object in its linear memory; modules that do not import it are unchanged.
- **Full manifest field table** with `deny_unknown_fields`, `PluginAuthor` accepting a string or an object, and a load-time warning for fields CodeRex accepts but does not implement.
- **`userConfig` injection**: `CODEREX_PLUGIN_OPTION_<KEY>` in the child's environment and `${user_config.KEY}` substitution in `env` and `entrypoint`; sensitive options refuse a plaintext default and refuse an unresolved `keyring://` reference; a required option with no value stops the spawn.
- **Install sources**: marketplace listing, local directory, git URL, GitHub `owner/repo` shorthand, a git subdirectory, and HTTPS archive — with `PluginSource::detect()` classifying a CLI argument, and the unimplemented `npm`/`command` kinds recognised and refused rather than silently reinterpreted.
- **Pinning**: trust-on-first-use SHA-256 for archives and commit pinning for git, recorded in `.coderex-pins.json`.

### Known Gaps

- No keychain access in this crate, so sensitive options are host-supplied or environment-supplied; nothing prompts the user or writes answers back to settings (see "userConfig Injection").
- Plugin channels route notifications from a named MCP server to subscribed sinks, but nothing in the MCP client drives `route_channel_notification` from a live notification stream yet (see "Channels").
- Pin files are created by the first install; a user cannot pre-seed a pin before installing.
- WASM plugins must import `coderex.user_config` to receive options; an existing module cannot be given `userConfig` without that ABI change.
- WASM compilation always requires a host-injected compiler command (or a `coderex-wasm-compile` next to the executable); with neither, loads fail closed. On Darwin the memory bound is `RLIMIT_DATA`, which does not cap the full address space; the Windows Job Object path is untested, and the compiler sandbox degrades to rlimits plus process-group kill where `sandbox-exec`/`bwrap` cannot be applied (see "WASM Plugins").
- `npm` and `command` sources are not implemented (see above for why, and for the reference citation on `npm`).
- `crates/cli/src/orchestrator.rs` parses `plugin.json` itself in `phase_init_plugins`; a parse error is now logged as a warning rather than silently ignored.

### Tests

- 169 tests in `plugins.rs` (manager ops, load/unload, marketplace, discovery, reconciler, URL validation, version checks, signature verification, dependency resolution, error recovery, custom endpoints, the manifest field table, `userConfig` resolution and injection end-to-end including the WASM required-option path, `userConfig` schema validation (`options`/`min`/`max`/`type`), plugin channels (live report states and notification routing), the plugin type registry, source detection and git URL validation, directory/git/git-subdir installs, structured source specs, the `npm`/`command` refusals, SHA-256 vectors and pin enforcement).
- 14 tests in `skills.rs` (parsing, loader, plugin/MCP skills, repo dir, conflict resolution, precedence, quoted strings).
- 7 tests in `runtime.rs` (spawn, initialize, terminate, invalid command).
- 20 tests in `wasm_runtime.rs`, all run by the default `cargo test -p coderex-plugins`: 4 core/unit tests (the loader stub, the injected command's program/leading-argument shape, helper discovery, and the platform rlimit plan), 10 sandbox tests (fuel, memory growth, initial memory, shared memory, WASI import, the 3 size-cap cases, `userConfig` injection into an importing module, and an empty-import module still running), and 6 pre-scan tests (normal module, oversized code section, too many functions, too many operators, excessive nesting, nesting exactly at the cap).
- 7 integration tests in `tests/wasm_compile_subprocess.rs`: an injected command compiles, serializes, and deserializes; leading arguments are forwarded; the host-injected global command is used; a missing command fails closed with an actionable error and leaves no artifacts; a slow helper and its spawned descendant are killed at the wall-clock limit; a second load hits the AOT cache with exactly one compiler invocation; and a pathological module is rejected by the pre-scan without starting the compiler.
- Tests use `tempfile` for filesystem operations. The git and git-subdir install tests create a throwaway repository inside a `tempfile::tempdir()` and pin every git command with `-C`, so they never touch the project's own repository. The subdirectory tests use `file://` clone URLs, so they need no network.
