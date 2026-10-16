# plugins Module Inspection

## Goal

Manages plugin lifecycle (discovery, loading, execution, unloading), marketplace connectivity (multiple marketplaces), and skill loading from multiple sources (bundled, user dir, repo dir, plugin, MCP).

## Design

### Key Types (plugins.rs)

- **`PluginState`** — Lifecycle enum: Discovered, Loading, Active, Error(String), Uninstalled, Installing.
- **`PluginHandle`** — Runtime representation with name, version, state, path, commands, skills, retry_count, max_retries, **verified**.
- **`PluginManifest`** — Deserialized from `plugin.json`. Identity/metadata (`name`, `displayName`, `version`, `description`, `author`, `homepage`, `repository`, `license`, `keywords`, `defaultEnabled`, `metadata`, `$schema`), CodeRex execution fields (`min_version`, `commands`, `skills`, `dependencies`, `signature`, `runtime`, `entrypoint`, `env`), `userConfig`, and the Claude component fields CodeRex accepts without acting on. See the field table below.
- **`PluginAuthor`** — `{name, email?}`; parses from either an object (Claude's shape) or a bare string.
- **`UserConfigField`** — One `userConfig` entry: `type`, `title`, `description`, `required`, `default`, `sensitive`, `multiple`, plus `options`/`min`/`max` accepted but not enforced.
- **`PluginSource`** — Install source: `Marketplace { name }`, `LocalDir(path)`, `Git { url, rev }`, `Archive { url, sha256 }`; `PluginSource::detect()` classifies a CLI argument.
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
- **Signature verification**: Ed25519 signature check on plugin archives (optional, configured via public key).
- **Manifest strictness**: `deny_unknown_fields` on `PluginManifest` and `UserConfigField`, plus a warning for accepted-but-unimplemented fields.
- **Environment injection**: `userConfig` values and static `env` are resolved and substituted before the child is spawned.
- **Trust on first use**: archive SHA-256 / git commit recorded in `.coderex-pins.json` and checked on later installs.
- **Dependency resolution**: Kahn's topological sort with cycle detection for plugin load ordering.
- **Error recovery**: Exponential-backoff retry for Error-state plugins (re-reads manifest, re-validates).
- **MCP skills bridge**: Discovered MCP tools converted to `SkillMetadata` and stored in `SessionContext.mcp_skills`.
- **Hybrid execution model**: Default tier = external process (JSON-RPC over stdio). Fast tier = WASM (reserved for future high-performance plugins).
- **Plugin command registration**: Dynamic `SlashCommand` registration via `Box::leak` for `&'static str` names/descriptions.
- **Plugin tool fallback**: `PluginToolExecutor` chains as a tertiary fallback behind primary registry -> MCP -> plugin.

### Public API Surface

- `PluginManager`, `PluginHandle`, `PluginState`, `PluginManifest`, `PluginAuthor`, `UserConfigField`, `RuntimeType`, `MarketplacePlugin`, `MarketplaceConnection`, `MarketplaceError`, `ReconciliationReport`, `MarketplaceReconciler`.
- `PluginSource`, `PluginPin`, `PluginPinStore`, `PIN_FILE_NAME`, `sha256_hex`, `substitute_user_config`, `PLUGIN_OPTION_ENV_PREFIX`, `PLUGIN_PUBLIC_KEY_ENV`, `SignatureCheck`.
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

WASM runtime is gated behind the `wasm-runtime` Cargo feature (`wasmtime` dependency). Currently, `spawn_plugin` logs a warning and skips WASM plugins. The infrastructure (manifest field, feature flag) is in place; the `wasmtime` integration is reserved for future high-performance, sandboxed plugins. `userConfig` is not injected into WASM plugins — there is no process environment to inject it into — and `spawn_plugin` says so out loud when a WASM manifest declares options.

## `plugin.json` Field Table

The manifest is parsed with `deny_unknown_fields`: a field that is not in the table below is a hard parse error naming the field, not something silently dropped. That was the real defect — an author could write `displayName` and be told nothing, because the whole field vanished at parse time. `discover()` likewise logs a warning for a manifest it cannot parse instead of skipping it in silence.

| Field | Status | Notes |
|---|---|---|
| `$schema`, `name`, `displayName`, `version`, `description`, `homepage`, `repository`, `license`, `keywords`, `defaultEnabled`, `metadata` | ✅ parsed | Identity and metadata. `author` accepts a string or `{name, email}`; an object used to fail the whole manifest. |
| `min_version`, `commands`, `skills`, `dependencies`, `signature`, `runtime`, `entrypoint`, `env` | ✅ acted on | CodeRex execution fields (pre-existing). |
| `userConfig` | ✅ implemented | See "userConfig injection". |
| `hooks` | 🟡 accepted, not implemented | CodeRex hooks come from settings, not from plugin manifests. |
| `agents`, `outputStyles`, `themes`, `workflows`, `monitors` | 🟡 accepted, not implemented | No CodeRex counterpart yet. |
| `mcpServers`, `lspServers`, `channels` | 🟡 accepted, not implemented | CodeRex's MCP servers are configured in settings; plugins cannot start their own. |
| `binaries`, `types`, `settings`, `experimental` | 🟡 accepted, not implemented | `binaries` carries per-binary SHA-256 pins in Claude; CodeRex pins archives and commits instead (see "Pinning"). |

Accepted-but-unimplemented fields are listed by `PluginManifest::unimplemented_fields()` and logged as a warning when the plugin loads, so "it parsed but does nothing" is visible rather than silent.

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
- Option keys must be usable as environment variable names (`[A-Za-z_][A-Za-z0-9_]*`); anything else is refused, because a key with a dash or an `=` in it is a way to smuggle a second variable into the environment.

**Sensitive options.** There is no keychain dependency in this crate (the workspace's keychain access lives in `coderex-config`, behind its `keyring` feature), so the sensitive path is strict rather than quiet:

- A `sensitive` option may **not** carry a plaintext `default` — a secret in `plugin.json` is a secret in a file that gets committed. That is a load-time error.
- A sensitive value that is still an unresolved `keyring://item` reference is refused: injecting the literal reference would hand the plugin a string that is not the secret, and resolving it needs the keychain. The host is expected to resolve it first (`coderex_config::resolve_api_key` is the existing convention).
- A `required` option with no value stops the spawn with an error naming the variable to set. A plugin that silently runs without the secret it asked for is worse than one that does not run.
- An optional option with no value is simply not injected (debug log).
- Sensitive values travel by environment variable, which is Claude's design too: visible to the plugin, and to the process table of the same user.

**Honest gap:** there is no interactive prompt and no plugin settings store, so options come from the host or from the environment — nothing writes a user's answers back to `settings.json` the way Claude does. There is also no option *validation* (`options`, `min`, `max`, `type` are parsed and ignored).

## Install Sources

`PluginSource` has four kinds, and `PluginSource::detect()` classifies a CLI argument into one of them (an existing directory → `LocalDir`; a git remote → `Git`; an HTTPS `.tar.gz`/`.tgz` → `Archive`; anything else → a marketplace listing name, which is what `coderex plugin install <name>` has always meant). `install_plugin` routes a listing whose url is not a marketplace URL to `install_from_source`, so `coderex plugin install ./my-plugin` and `coderex plugin install https://github.com/acme/plugin.git` work without the CLI changing its argument shape.

| Kind | Behaviour |
|---|---|
| `Marketplace { name }` | The original path: HTTPS listing → download endpoint → extract → validate → atomic rename. |
| `LocalDir(path)` | Reads `plugin.json`, copies the directory into a staging directory, then renames it into place. The source is copied, not moved or linked, so it stays a working copy. |
| `Git { url, rev }` | Shallow clone (`--depth 1`, `--branch <rev>`); a full commit SHA as `rev` clones the history and checks it out. `.git` is dropped afterwards so the install is a snapshot, not a nested repository. The resolved commit is the plugin's pin. |
| `Archive { url, sha256 }` | HTTPS only. Signature-checked like a marketplace archive, and the archive's SHA-256 is recorded as a pin. |

Git transports that run commands are refused before `git` is spawned: `ext::` and any other `<name>::` transport is rejected, and only `https://`, `ssh://`, `git://`, `file://` and `user@host:path` are accepted. Credential prompts are disabled (`GIT_TERMINAL_PROMPT=0`) so an unauthenticated clone fails with a message instead of hanging, and `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` are not inherited from the caller.

### Which of Claude's source kinds do not apply

Claude's marketplace entry union has eight-ish kinds. CodeRex implements four of the shapes and deliberately not the rest:

| Claude kind | CodeRex | Why |
|---|---|---|
| local path | ✅ `LocalDir` | Local-first tool; this is the primary source. |
| `url`, `github`, `git-subdir` | ✅ `Git` (one kind) | All three are "clone a git remote at a ref"; `rev` covers the ref, and `detect()` recognises GitHub/GitLab/Codeberg/Bitbucket URLs. Subdirectory extraction (`git-subdir`'s sparse checkout) is not implemented — the repository root must hold `plugin.json`. |
| `archive` | ✅ `Archive` | Straightforward and gives the SHA-256 pin a home. |
| `npm` | ❌ | Needs a Node toolchain and registry resolution; CodeRex plugins are not npm packages. Claude itself still throws "NPM marketplace sources not yet implemented". |
| `command` | ❌ | Runs an arbitrary command to print a directory path. CodeRex refuses to execute a command as part of resolving a source; a local path or a git URL says the same thing without the execution. |
| `unsupported` | ❌ | Claude's placeholder variant; nothing to implement. |
| `settings` | ❌ | Claude-internal (plugins defined inside settings); CodeRex's settings have no plugin table to read. |

## Pinning (Trust on First Use)

A configured signing key is the strong check, but a plugin installed with no key configured was previously `Unverified` with only a warning. CodeRex now also records what it installed, in `.coderex-pins.json` inside the install directory:

- **Archive installs** record the archive's SHA-256.
- **Git installs** record the resolved commit.
- **Directory installs** record nothing: a directory has no archive and no commit, and what protects it is that it is a path the user chose.

On a later install of the same plugin name, the recorded pin must match, or the install is refused. A pin file that exists but cannot be parsed is an error rather than an empty store — silently forgetting the pins would turn the next install into a fresh first use. A pin that cannot be *written* after a successful install is a warning: the plugin is in place and correct, but the protection it would have had is missing.

**Honest gaps:** this is trust-on-first-use, not a signature. There is no pin *file* a user can pre-seed to pin before the first install (the pins are created by the first install), and no way to pin a directory source. A `PluginSource::Archive { sha256 }` caller can pin explicitly for that install.

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
- Auto-install new plugins from marketplace.
- Skill loading from all 5 sources with conflict resolution.
- Skill frontmatter parsing via `serde_yaml` with custom scalar/array deserializers.
- Repo-specific skills from `.coderex/skills/`.
- MCP skills bridge: `phase_init_mcp` converts discovered MCP tools to `SkillMetadata`.
- **WASM feature flag prep**: `process-runtime` (default) and `wasm-runtime` Cargo features.
- **Full manifest field table** with `deny_unknown_fields`, `PluginAuthor` accepting a string or an object, and a load-time warning for fields CodeRex accepts but does not implement.
- **`userConfig` injection**: `CODEREX_PLUGIN_OPTION_<KEY>` in the child's environment and `${user_config.KEY}` substitution in `env` and `entrypoint`; sensitive options refuse a plaintext default and refuse an unresolved `keyring://` reference; a required option with no value stops the spawn.
- **Install sources**: marketplace listing, local directory, git URL, HTTPS archive — with `PluginSource::detect()` classifying a CLI argument.
- **Pinning**: trust-on-first-use SHA-256 for archives and commit pinning for git, recorded in `.coderex-pins.json`.

### Known Gaps

- No keychain access in this crate, so sensitive options are host-supplied or environment-supplied; nothing prompts the user or writes answers back to settings (see "userConfig Injection").
- `options`/`min`/`max`/`type` in `userConfig` are parsed and ignored: no validation.
- No subdirectory extraction for git sources (`git-subdir`): the repository root must hold `plugin.json`.
- Pin files are created by the first install; a user cannot pre-seed a pin before installing.
- WASM plugins get no `userConfig` (no process environment).
- `crates/cli/src/orchestrator.rs` still parses `plugin.json` itself in `phase_init_plugins` and ignores a parse error silently; the library's `discover()` warns, but the CLI's own loop has not been changed here.

### Tests

- 58 tests in `plugins.rs` (manager ops, load/unload, marketplace, discovery, reconciler, URL validation, version checks, signature verification, dependency resolution, error recovery, custom endpoints, the manifest field table, `userConfig` resolution and injection end-to-end, source detection and git URL validation, directory/git installs, SHA-256 vectors and pin enforcement).
- 16+ tests in `skills.rs` (parsing, loader, plugin/MCP skills, repo dir, conflict resolution, precedence, quoted strings).
- 6 tests in `runtime.rs` (spawn, initialize, terminate, invalid command).
- Tests use `tempfile` for filesystem operations. The git install test creates a throwaway repository inside a `tempfile::tempdir()` and pins every git command with `-C`, so it never touches the project's own repository.
