# config Module Inspection

## Goal

Three-level configuration system: defaults → user config (`~/.coderex/settings.json`) → project config (`.coderex/settings.json`). Manages settings, feature flags, experiments, and enterprise policies.

## Design

### Key Types

- **`ConfigService`** (`settings.rs`) — Loads and merges configuration from all three levels.
- **`Settings`** (`settings.rs`) — Strongly-typed configuration struct.
- **`RuntimeOverrides`** (`settings.rs`) — Environment-variable and CLI-arg overrides.
- **`ConfigError`** (`settings.rs`) — Error enum for config operations.
- **`config_home()`** (`settings.rs`) — Returns `~/.coderex` (was `~/.claude`).
- **`FeatureFlags`** (`flags.rs`) — Toggleable features.
- **`Experiments`** (`experiments.rs`) — A/B experiment configuration.

### Configuration Hierarchy

```
Defaults (compiled)
  ↓ override
~/.coderex/settings.json (user-level)
  ↓ override
.coderex/settings.json (project-level)
  ↓ override
Environment variables / CLI args
```

### Enterprise Features (gated)

- `remote_settings.rs` — Fetch managed settings from corporate server.
- `org_policy.rs` — Organizational policy enforcement.
- `trusted_device.rs` — Device fingerprinting and trust registry.

## Current Status

### What Works

- Three-level config merge with precedence.
- JSON config parsing with serde.
- `config_home()` returning `~/.coderex`.
- Feature flags and experiments modules.
- Enterprise modules compile behind feature gate.

### What Is Stubbed/Partial

- **Remote settings fetch** — HTTP client exists but may not handle auth.
- **Org policy enforcement** — Policies can be loaded but not actively enforced.
- **Trusted device registry** — Fingerprinting works but no actual trust validation flow.

## Gaps for Production

- **No config validation**: Invalid JSON or unknown keys are silently ignored.
  → **Suggestion**: Add a `#[serde(deny_unknown_fields)]` attribute to `Settings` to reject unknown keys. Implement a `validate()` method that checks constraints (e.g., `batch_size > 0`, `timeout_seconds >= 1`). Return `ConfigError::Validation` with descriptive messages.
- **No schema migration**: Config format changes break old configs without warning.
  → **Suggestion**: Add a `config_version: u32` field to `Settings`. On load, if the stored version < current version, run a migration chain (e.g., `v1_to_v2`, `v2_to_v3`). Write the migrated config back to disk.
- **No config diff/inspect**: Cannot see which settings differ from defaults.
  → **Suggestion**: Add a `/config diff` command that computes a JSON diff between the merged config and compiled defaults. Show only changed keys with their source (user/project/env).
- **No encrypted secrets**: API keys in config are plaintext.
  → **Suggestion**: Store secrets in the OS keychain via `keyring` crate. In `settings.json`, use `"api_key": "keyring://coderex-api-key"` as a reference. Resolve the reference at load time. Never write raw keys to JSON.
- **Remote settings no caching**: Fetches on every startup; no offline mode.
  → **Suggestion**: Cache the last successful remote settings fetch in `~/.coderex/remote_settings_cache.json` with a timestamp. On startup, serve from cache if the remote is unreachable or the cache is younger than the TTL.
- **No config hot-reload**: Changes require process restart.
  → **Suggestion**: Use `notify` crate to watch config files for changes. On modification, re-parse and validate, then emit a `ConfigReloaded` event to subscribers (UI, engine, tools). Roll back on validation failure.
- **No per-project feature flags**: Flags are global, not scoped to workspaces.
  → **Suggestion**: Extend `FeatureFlags` to support a `project_overrides: HashMap<PathBuf, FeatureFlags>` map. On config load, apply workspace-specific flags based on the current working directory.
