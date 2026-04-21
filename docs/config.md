# config Module Inspection

## Goal

Three-level configuration system: defaults → user config (`~/.coderex/settings.json`) → project config (`.coderex/settings.json`). Manages settings, feature flags, experiments, and enterprise policies.

## Design

### Key Types

- **`ConfigService`** (`settings.rs`) — Loads and merges configuration from all three levels.
- **`Settings`** (`settings.rs`) — Strongly-typed configuration struct with `config_version`, `api_key`, and validation.
- **`RuntimeOverrides`** (`settings.rs`) — Environment-variable and CLI-arg overrides.
- **`ConfigError`** (`settings.rs`) — Error enum with `Io`, `Json`, `Validation`, and `UnknownField` variants.
- **`ConfigDiff`/`ConfigDiffEntry`** (`settings.rs`) — Structured diff between current settings and defaults with `render()` for human-readable output.
- **`ConfigWatcher`** (`settings.rs`) — File watcher (feature-gated on `hot-reload`) that emits change events via watch channel.
- **`config_home()`** (`settings.rs`) — Returns `~/.coderex`.
- **`FeatureFlags`** (`flags.rs`) — Toggleable features with project-level overrides.
- **`Experiments`** (`experiments.rs`) — A/B experiment configuration with disk caching.
- **`resolve_api_key()`** (`settings.rs`) — Resolves `keyring://` URI references to OS keychain secrets.
- **`migrate_settings()`** (`settings.rs`) — Schema migration chain (v0 → v1 → v2).

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

- `remote_settings.rs` — Fetch managed settings from corporate server with disk cache for offline fallback.
- `org_policy.rs` — Organizational policy enforcement with model allowlists, tool blocks, and budget caps.
- `trusted_device.rs` — Device fingerprinting and trust registry with org scoping.

## Current Status

The `coderex-config` crate is fully implemented with no stubs or gaps for production.
