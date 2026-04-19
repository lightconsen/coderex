# plugins Module Inspection

## Goal

Manages plugin lifecycle (discovery, loading, unloading), marketplace connectivity (multiple marketplaces), and skill loading from multiple sources (bundled, user dir, repo dir, plugin, MCP).

## Design

### Key Types (plugins.rs)

- **`PluginState`** — Lifecycle enum: Discovered, Loading, Active, Error(String), Uninstalled, Installing.
- **`PluginHandle`** — Runtime representation with name, version, state, path, commands, skills.
- **`PluginManifest`** — Deserialized from `plugin.json` (name, version, description, author, min_version, commands, skills).
- **`MarketplacePlugin`** — Marketplace listing with download URL.
- **`MarketplaceConnection`** — HTTP client + base URL per marketplace.
- **`PluginManager`** — `DashMap`-backed registry of plugins and marketplaces.
- **`ReconciliationReport`** — Diff between local and marketplace state.
- **`MarketplaceReconciler`** — Background reconciliation with optional auto-install.

### Key Types (skills.rs)

- **`SkillMetadata`** — Parsed frontmatter: name, hooks, agent, allowed_tools, description, enabled.
- **`Skill`** — Loaded skill with metadata, source, path.
- **`SkillSource`** — Enum: Bundled, UserDir, RepoDir, Plugin, Mcp.
- **`SkillFile`** — Parsed content: metadata + script body.
- **`SkillLoader`** — Loads `.md` skill files from configured directories.

### Architecture Patterns

- **DashMap** for concurrent plugin/marketplace registries.
- **AtomicBool** for refresh flag.
- **Frontmatter parsing** with custom YAML-like parser (not full YAML).
- **Layered skill loading**: bundled → user → repo (`.coderex/skills`).

### Public API Surface

- `PluginManager`, `PluginHandle`, `PluginState`, `PluginManifest`, `MarketplacePlugin`, `MarketplaceConnection`, `ReconciliationReport`, `MarketplaceReconciler`.
- `SkillLoader`, `Skill`, `SkillMetadata`, `SkillSource`, `SkillFile`, `parse_skill_file`.

## Current Status

### What Works

- Plugin discovery from directories (reads `plugin.json`).
- Plugin load/unload with state tracking.
- Marketplace registration and HTTP fetching.
- Plugin installation (download, write manifest + archive).
- Reconciliation: detect new, updated, removed plugins.
- Auto-install new plugins from marketplace.
- Skill loading from all 5 sources.
- Skill frontmatter parsing with arrays and scalar values.
- Repo-specific skills from `.coderex/skills/`.

### What Is Stubbed/Partial

- **Plugin installation does not extract the archive**: Downloads `plugin.tar.gz` but never unpacks it (`plugins.rs:326`).
- **No actual plugin execution**: Commands and skills are registered as strings but no runtime invokes them.
- **No version compatibility check**: `min_version` field in manifest is parsed but never enforced.
- **No dependency resolution**: Plugins cannot depend on other plugins.
- **MCP skills are stubs**: `add_mcp_skill` exists but no actual MCP discovery.
- **Error state recovery**: Plugins enter `Error` state but no retry logic.

### Tests

- 17 tests in `plugins.rs` (manager ops, load/unload, marketplace, discovery, reconciler).
- 13 tests in `skills.rs` (parsing, loader, plugin/MCP skills, repo dir).
- Tests use `tempfile` for filesystem operations.

## Gaps for Production

- **SECURITY — No signature verification**: Downloaded plugins are not verified; arbitrary code can be installed from any marketplace.
  → **Suggestion**: Require every plugin to be signed with Ed25519. Distribute the marketplace public key in the binary. On install, verify the signature of `plugin.tar.gz` before extraction. Reject unsigned or invalid-signature plugins.
- **SECURITY — No sandboxing**: Plugin code would run with full process privileges.
  → **Suggestion**: Execute plugins in a WASM runtime (`wasmtime` or `wasmer`) with a capability-based API. Grant filesystem access only to a plugin-specific directory. Deny network access unless explicitly requested in the manifest.
- **SECURITY — Marketplace URL injection**: No validation of marketplace base URLs.
  → **Suggestion**: Maintain an allowlist of trusted marketplace domains (e.g., `marketplace.coderex.dev`). Reject registration of marketplaces with non-HTTPS or non-allowlisted URLs. Validate URLs with `url` crate.
- **Hardcoded endpoint paths**: `/api/plugins` and `/api/plugins/{name}/download` are hardcoded.
  → **Suggestion**: Make endpoint paths configurable per `MarketplaceConnection` with sensible defaults. Allow marketplaces to advertise their API structure via a `.well-known/coderex-marketplace.json` discovery document.
- **Archive not extracted**: `plugin.tar.gz` is written but never unpacked — plugin is non-functional after install.
  → **Suggestion**: Use `tar` + `flate2` crates to extract the archive after download. Validate the extracted structure (must contain `plugin.json` at the root). Clean up the `.tar.gz` after successful extraction.
- **No rollback on failed install**: Partial writes leave broken state.
  → **Suggestion**: Install plugins into a temporary directory first (e.g., `plugins/.install-{name}/`). Only atomically rename to the final directory after all validation passes (signature check, manifest parse, structure validation). On failure, delete the temp directory.
- **No plugin unloading cleanup**: `unload_plugin` removes from map but does not clean up resources.
  → **Suggestion**: Add a `Plugin::cleanup()` method that closes file handles, stops background tasks, and removes temporary directories. Call it before removing the plugin from the registry.
- **No concurrent install protection**: Multiple installs of same plugin could race.
  → **Suggestion**: Use a `DashMap<String, Mutex<()>>` as an install lock. Before installing, acquire the lock for the plugin name. Return `PluginState::Installing` if another thread is already installing it.
- **Skill frontmatter parser is fragile**: Custom parser, not real YAML; fails on complex nested structures, quoted strings with colons, multiline values.
  → **Suggestion**: Replace the custom parser with a minimal YAML frontmatter extractor (split on `---`, then parse the header with `serde_yaml`). This handles all standard YAML constructs without reimplementing a parser.
- **No skill script execution**: Script content is loaded but never interpreted or executed.
  → **Suggestion**: Define a skill execution model: skills are Rust functions registered at compile time or WASM functions loaded dynamically. Parse the script body as a domain-specific language or map skill names to built-in handler functions.
- **No skill conflict resolution**: Two skills with same name from different sources — last loaded wins silently.
  → **Suggestion**: Add a conflict detection pass after loading all skills. If duplicates are found, prefer `UserDir > RepoDir > Bundled > Plugin > Mcp` by default. Log a warning showing which skill was overridden and why. Allow users to configure the precedence order.
