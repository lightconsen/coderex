# config Module Inspection

## Goal

Configuration layers: defaults → user config (`~/.coderex/settings.json`) → project config (`.coderex/settings.json`) → local config (`.coderex/settings.local.json`), plus a managed/enterprise layer. Manages settings, feature flags, experiments, and enterprise policies.

## Design

### Key Types

- **`ConfigService`** (`settings.rs`) — Loads and merges configuration from all three levels.
- **`Settings`** (`settings.rs`) — Strongly-typed configuration struct with `config_version`, `api_key`, `context_window`, and validation.
- **`ContextWindowSettings`** (`settings.rs`) — Per-model context-window overrides plus an optional default.
- **`RuntimeOverrides`** (`settings.rs`) — Environment-variable and CLI-arg overrides.
- **`ConfigError`** (`settings.rs`) — Error enum with `Io`, `Json`, `Validation`, `UnknownField`, and `TooLarge` variants.
- **`ConfigDiff`/`ConfigDiffEntry`** (`settings.rs`) — Structured diff between current settings and defaults with `render()` for human-readable output.
- **`ConfigWatcher`** (`settings.rs`) — File watcher (feature-gated on `hot-reload`) that emits change events via watch channel.
- **`config_home()`** (`settings.rs`) — Returns `~/.coderex`.
- **`managed.rs`** — OS-level managed-settings carriers. `load_managed_plist` converts a macOS MDM property list (via the `plist` crate) to `Settings`; `load_managed_registry` reads `HKLM\SOFTWARE\CodeRex` (via `winreg`, Windows-only) and converts registry values to `Settings`. `default_managed_plist_path`, `DEFAULT_MACOS_MANAGED_PLIST_DOMAIN` and `WINDOWS_MANAGED_REGISTRY_KEY` expose the defaults.
- **`TelemetrySubConfig`** (`settings.rs`) — Endpoint/batch/sampling settings plus optional export-channel settings: `otlp_endpoint`, `otlp_headers`, `datadog_api_key`, `datadog_site`, `datadog_endpoint` (see [`docs/telemetry.md`](telemetry.md#channels)).
- **`FeatureFlags`** (`flags.rs`) — Toggleable features with project-level overrides, including context-window experiment values.
- **`Experiments`** (`experiments.rs`) — A/B experiment configuration with disk caching.
- **`resolve_api_key()`** (`settings.rs`) — Resolves `keyring://` URI references to OS keychain secrets.
- **`migrate_settings()`** (`settings.rs`) — Schema migration chain (v0 → v1 → v2 → v3 → v4).

### Configuration Hierarchy

```
Defaults (compiled)
  ↓ override
~/.coderex/settings.json (user-level)
  ↓ override
.coderex/settings.json (project-level)
  ↓ override
.coderex/settings.local.json (local, gitignored by convention; written by /init)
  ↓ override
managed sources (managed_settings.json → macOS plist → Windows registry; see below)
  ↓ override
Environment variables / CLI args
```

Every key is **winner-take-all** under merge: a higher layer replaces the whole
value rather than merging into it (the `telemetry` sub-config is replaced
wholesale, not field-by-field). Claude's `WINNER_ONLY_UNDER_MERGE` key table
therefore has no counterpart here — the property already holds for all keys, and
no key is excluded from it.

### Context Window Overrides

`context_window` maps model names to context-window sizes in tokens and may
carry a `default` for models without a per-model entry:

```json
{
  "context_window": {
    "default": 64000,
    "models": {
      "my-gateway-alias": 200000,
      "claude-sonnet-4-6": 200000
    }
  }
}
```

The value flows through every settings source and the normal merge chain.
Validation rejects zero windows and blank model names, so a mistyped value
cannot turn compaction off.

`Settings::context_window_overrides()` converts the field into the
`coderex_core::types::ContextWindowOverrides` type used by the resolver.
Resolution precedence, environment variables, the client-data file
(`~/.coderex/context_windows.json`), experiment syntax, and the built-in model
table are documented in [`docs/compact.md`](compact.md).

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

Managed carriers are merged in a fixed order, lowest priority first. Every
carrier is finalized as the `"managed"` source, so (unlike project settings) it
may enable telemetry and security-sensitive permission modes. All carriers go
through the same unknown-key check, migration and validation as a normal
settings file.

| Carrier | Status |
|---|---|
| Remote HTTP | Implemented — `remote_settings.rs` (checksum-verified fetch, disk cache, fail-open, `LoadingBarrier`). |
| Managed file | Implemented — `ConfigService::with_managed_settings_path` reads a `ManagedSettings` document, verifies its SHA-256 checksum (mismatch ⇒ ignored, fail closed), and merges it after user/project settings. Default path `<config_dir>/managed_settings.json`, overridable with `CODEREX_MANAGED_SETTINGS_PATH` so it can point at an admin-controlled location. |
| macOS plist (MDM) | Implemented — `ConfigService::with_managed_settings_plist(path)` reads a property list with the `plist` crate; `with_default_macos_managed_plist()` selects `/Library/Managed Preferences/dev.coderex.plist`. The top level must be a dictionary of settings keys (`model`, `permission_mode`, `telemetry_enabled`, `telemetry`, …). Merged after the managed file. No checksum is verified (the OS protects `/Library/Managed Preferences`); the file is still size-capped, validated, and rejected whole on an unknown key. |
| Windows HKLM registry | Implemented, **unverified** — `ConfigService::with_managed_settings_registry_key(path)` (or `with_default_windows_managed_registry()` for `HKLM\SOFTWARE\CodeRex`) reads values via `winreg`. Merged last. The reader is compile-gated to Windows and has not been executed on the macOS development machine; its pure value-conversion layer is unit-tested on every platform. |

#### Windows registry value mapping

The registry has no boolean or float type, so a few keys are normalized:

| Key | Registry type | Mapping |
|---|---|---|
| `model`, `permission_mode`, `api_key` | `REG_SZ` | string |
| `telemetry_enabled` | `REG_DWORD` or `REG_SZ` | `0`/`false` ⇒ `false`, non-zero/`true` ⇒ `true` |
| `config_version` | `REG_DWORD`/`REG_QWORD`, or numeric `REG_SZ` | unsigned integer |
| `max_budget_usd` | `REG_SZ` | decimal number |
| `telemetry`, `hooks`, `mcp_servers`, `context_window` | `REG_SZ` | JSON value |
| other supported names | `REG_SZ`, `REG_DWORD`, `REG_QWORD`, `REG_MULTI_SZ` | string, number, number, array |

Unknown value names and unparseable values cause the whole registry carrier to
be skipped with a warning — a typo must not silently produce a partial policy.
Other value types (for example `REG_BINARY`) are ignored. Missing keys are
treated as absent.

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
and `preserveOnWrite` are deliberate ⚪ (see above). The macOS plist carrier is
implemented and tested on macOS; the Windows registry carrier is implemented and
compile-gated but **unverified** — it has not run on this development machine.

### Export-channel settings

`telemetry` may carry the optional OTLP and Datadog channel settings:

```json
{
  "telemetry": {
    "otlp_endpoint": "http://localhost:4318",
    "otlp_headers": "x-api-key=collector-key",
    "datadog_api_key": "…",
    "datadog_site": "datadoghq.eu",
    "datadog_endpoint": "https://http-intake.logs.datadoghq.eu/api/v2/logs"
  }
}
```

The telemetry crate reads the equivalent `CODEREX_OTLP_*` / `CODEREX_DATADOG_*`
environment variables directly (see [`docs/telemetry.md`](telemetry.md#channels));
embedders that construct `coderex_telemetry::ChannelConfig` can pass these
settings explicitly with `TelemetryUploader::with_channels`.

### Tests

`cargo test -p coderex-config` covers:

- macOS plist loading, unknown-key rejection, missing-file handling, and
  precedence over the managed JSON file (compile-gated to macOS).
- registry value conversion (`REG_SZ`/`REG_DWORD`/`REG_QWORD`/`REG_MULTI_SZ`,
  boolean/float/JSON normalization, invalid values), on every platform.
- registry carrier inertness on non-Windows.
