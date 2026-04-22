# plugins Module Inspection

## Goal

Manages plugin lifecycle (discovery, loading, unloading), marketplace connectivity (multiple marketplaces), and skill loading from multiple sources (bundled, user dir, repo dir, plugin, MCP).

## Design

### Key Types (plugins.rs)

- **`PluginState`** — Lifecycle enum: Discovered, Loading, Active, Error(String), Uninstalled, Installing.
- **`PluginHandle`** — Runtime representation with name, version, state, path, commands, skills, retry_count, max_retries.
- **`PluginManifest`** — Deserialized from `plugin.json` (name, version, description, author, min_version, commands, skills, dependencies, signature).
- **`MarketplacePlugin`** — Marketplace listing with download URL.
- **`MarketplaceConnection`** — HTTP client + base URL + configurable endpoint paths per marketplace.
- **`MarketplaceError`** — URL validation errors: InsecureUrl, InvalidUrl, NotAllowed.
- **`PluginManager`** — `DashMap`-backed registry of plugins and marketplaces with install locks, version checks, signature verification, and dependency resolution.
- **`ReconciliationReport`** — Diff between local and marketplace state.
- **`MarketplaceReconciler`** — Background reconciliation with optional auto-install.

### Key Types (skills.rs)

- **`SkillMetadata`** — Parsed frontmatter: name, hooks, agent, allowed_tools, description, enabled.
- **`Skill`** — Loaded skill with metadata, source, path.
- **`SkillSource`** — Enum: Bundled, UserDir, RepoDir, Plugin, Mcp. Precedence ordering for conflict resolution.
- **`SkillFile`** — Parsed content: metadata + script body.
- **`SkillLoader`** — Loads `.md` skill files from configured directories with conflict resolution.

### Architecture Patterns

- **DashMap** for concurrent plugin/marketplace registries.
- **AtomicBool** for refresh flag.
- **YAML frontmatter parsing** via `serde_yaml` with custom `deserialize_string_or_list` for scalar/array fields.
- **Layered skill loading**: bundled → user → repo (`.coderex/skills`) with precedence-based conflict resolution.
- **Atomic install staging**: temp directory + validation + atomic rename + cleanup on failure.
- **Install locks**: `DashMap<String, Arc<tokio::sync::Mutex<()>>>` prevents concurrent installs.
- **Version compatibility**: SemVer comparison of `min_version` against app version on load and install.
- **Signature verification**: Ed25519 signature check on plugin archives (optional, configured via public key).
- **Dependency resolution**: Kahn's topological sort with cycle detection for plugin load ordering.
- **Error recovery**: Exponential-backoff retry for Error-state plugins (re-reads manifest, re-validates).
- **MCP skills bridge**: Discovered MCP tools converted to `SkillMetadata` and stored in `SessionContext.mcp_skills`.

### Public API Surface

- `PluginManager`, `PluginHandle`, `PluginState`, `PluginManifest`, `MarketplacePlugin`, `MarketplaceConnection`, `MarketplaceError`, `ReconciliationReport`, `MarketplaceReconciler`.
- `SkillLoader`, `Skill`, `SkillMetadata`, `SkillSource`, `SkillFile`, `parse_skill_file`.

## Current Status

### What Works

- Plugin discovery from directories (reads `plugin.json`).
- Plugin load/unload with state tracking and version compatibility checks.
- Marketplace registration with HTTPS enforcement and optional domain allowlist.
- Configurable marketplace endpoint paths (`list_endpoint`, `download_endpoint`) with defaults.
- Plugin installation: download → temp staging → archive extraction (tar+gzip) → validation → atomic rename → cleanup.
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

### What Is Stubbed/Partial

*None remaining — all implementation gaps closed.*

### Tests

- 30 tests in `plugins.rs` (manager ops, load/unload, marketplace, discovery, reconciler, URL validation, version checks, signature verification, dependency resolution, error recovery, custom endpoints).
- 16 tests in `skills.rs` (parsing, loader, plugin/MCP skills, repo dir, conflict resolution, precedence, quoted strings).
- Tests use `tempfile` for filesystem operations.

## Gaps for Production

*None remaining — all implementation gaps closed.*

## Remaining Considerations

The following are architectural design decisions, not missing implementation:

- **Plugin execution runtime**: How plugin commands and skills are executed (WASM runtime, DSL interpreter, or compile-time registration). The infrastructure (discovery, loading, dependency resolution, sandboxing hooks) is in place; an execution model needs to be chosen.
- **Skill script execution**: The script body is loaded and parsed but not interpreted. A skill execution model (WASM functions, built-in handler registry, or DSL) needs to be defined.
- **WASM sandboxing**: Plugin execution isolation via a WASM runtime (`wasmtime`) with capability-based API. The signature verification and dependency infrastructure exists; the runtime integration is a future architecture decision.
