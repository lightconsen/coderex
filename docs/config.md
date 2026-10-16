# config Module Inspection

## Goal

Three-level configuration system: defaults → user config (`~/.coderex/settings.json`) → project config (`.coderex/settings.json`), plus a managed/enterprise layer. Manages settings, feature flags, experiments, and enterprise policies.

## Design

### Key Types

- **`ConfigService`** (`settings.rs`) — Loads and merges configuration from all three levels.
- **`Settings`** (`settings.rs`) — Strongly-typed configuration struct with `config_version`, `api_key`, and validation.
- **`RuntimeOverrides`** (`settings.rs`) — Environment-variable and CLI-arg overrides.
- **`ConfigError`** (`settings.rs`) — Error enum with `Io`, `Json`, `Validation`, `UnknownField`, and `TooLarge` variants.
- **`ConfigDiff`/`ConfigDiffEntry`** (`settings.rs`) — Structured diff between current settings and defaults with `render()` for human-readable output.
- **`ConfigWatcher`** (`settings.rs`) — File watcher (feature-gated on `hot-reload`) that emits change events via watch channel.
- **`config_home()`** (`settings.rs`) — Returns `~/.coderex`.
- **`FeatureFlags`** (`flags.rs`) — Toggleable features with project-level overrides.
- **`Experiments`** (`experiments.rs`) — A/B experiment configuration with disk caching.
- **`resolve_api_key()`** (`settings.rs`) — Resolves `keyring://` URI references to OS keychain secrets.
- **`migrate_settings()`** (`settings.rs`) — Schema migration chain (v0 → v1 → v2 → v3).

### Configuration Hierarchy

```
Defaults (compiled)
  ↓ override
~/.coderex/settings.json (user-level)
  ↓ override
.coderex/settings.json (project-level)
  ↓ override
managed_settings.json (managed/enterprise file carrier, see below)
  ↓ override
Environment variables / CLI args
```

Every key is **winner-take-all** under merge: a higher layer replaces the whole
value rather than merging into it (the `telemetry` sub-config is replaced
wholesale, not field-by-field). Claude's `WINNER_ONLY_UNDER_MERGE` key table
therefore has no counterpart here — the property already holds for all keys, and
no key is excluded from it.

### Source Policies (repository-controlled files cannot relax privacy)

`load_settings_file` applies two policies after parsing, both keyed on the
settings *source*:

- **`permission_mode`** — project/local settings may not set `auto` or
  `bypass-permissions`; the value is dropped so the next layer wins.
- **`telemetry_enabled`** — project/local settings may only **disable**
  telemetry. An attempt to set `true` is dropped, so a repository-committed
  `.coderex/settings.json` can never turn telemetry on for a user who has not
  consented. User, flag, and managed sources may enable it.

### Invalid Files and Size Cap

A settings file is applied **whole or not at all**: an oversized file
(> `MAX_SETTINGS_FILE_BYTES`, 2 MiB), an unknown key, invalid JSON, or a failed
validation rejects the entire file, and `ConfigService::resolve` falls back to
the next layer. A file is never partially applied.

One exception is the **restrictive re-read**: an unrecognized `permission_mode`
is dropped (with a warning) rather than rejecting the file, so the remaining keys
still apply and the effective mode falls back to the most restrictive default
(`"default"`). Every other invalid value rejects the whole file.

### Enterprise Carriers

| Carrier | Status |
|---|---|
| Remote HTTP | Implemented — `remote_settings.rs` (checksum-verified fetch, disk cache, fail-open, `LoadingBarrier`). |
| Managed file | Implemented — `ConfigService::with_managed_settings_path` reads a `ManagedSettings` document, verifies its SHA-256 checksum (mismatch ⇒ ignored, fail closed), and merges it last. Default path `<config_dir>/managed_settings.json`, overridable with `CODEREX_MANAGED_SETTINGS_PATH` so it can point at an admin-controlled location. |
| macOS plist | ⚪ Not implemented — would need a plist parser dependency and an MDM-style fixed path; the file carrier covers the same use on this platform. |
| Windows HKLM registry | ⚪ Not implemented — Windows-only; CodeRex targets macOS/Linux. |

`org_policy.rs` (model allowlists, tool blocks, budget caps) is an in-memory
enforcement layer with no runtime consumer wired yet — it is not a settings
carrier.

**Not copied from Claude:** `preserveOnWrite` for managed-only keys. In Claude,
managed keys can live in the same `settings.json` the CLI rewrites, so a
non-managed write could delete an admin key. In CodeRex, managed settings are a
separate source (file/remote) applied *after* user/project settings, so a
rewrite of a user or project file can never delete a managed value. The
mechanism has no trigger here, so adding it would be dead code.

## Current Status

The `coderex-config` crate is implemented with no stubs. `WINNER_ONLY_UNDER_MERGE`
and `preserveOnWrite` are deliberate ⚪ (see above); the plist and registry
carriers are deliberate ⚪.
