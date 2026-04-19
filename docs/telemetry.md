# telemetry Module Inspection

## Goal

Privacy-first telemetry collection, local storage, and batch upload. Stores events in SQLite locally, uploads in batches when online, and captures crash reports via panic hooks.

## Design

### Key Types (lib.rs)

- **`TelemetryHandle`** — `mpsc::Sender` wrapper for thread-safe event recording.
- **`TelemetryError`** — Error enum: Database, Io, Serialization, Upload.

### Key Types (store.rs)

- **`TelemetryStore`** — SQLite connection wrapper with schema management.
- **`TelemetryEvent`** — Enum: SessionStart, ToolUsed, ApiCall, Error (with chrono timestamps).
- **`PendingEvent` / `PendingCrash`** — Rows pending upload.

### Key Types (uploader.rs)

- **`TelemetryConfig`** — Endpoint, API key, batch size (default 100), interval (default 300s), enabled flag.
- **`TelemetryUploader`** — reqwest client + store + config.

### Key Types (crash.rs)

- **`CrashReport`** — Serializable crash data.
- **`CrashReporter`** — Panic hook + pending directory ingestion.

### Architecture Patterns

- **Producer-consumer**: `TelemetryHandle` (sender) → worker thread (receiver) → SQLite.
- **Batching**: 32-event batches or 30-second flush intervals.
- **WAL mode SQLite** for concurrent reads.
- **Retention trigger**: deletes oldest 100 events when table exceeds 10000 rows.
- **Crash reports written to filesystem** during panic (because SQLite is not Sync), then ingested on next startup.

### Public API Surface

- `TelemetryHandle`, `TelemetryEvent`, `TelemetryStore`, `TelemetryUploader`, `TelemetryConfig`, `CrashReporter`, `CrashReport`, `PendingEvent`, `PendingCrash`.

## Current Status

### What Works

- SQLite schema creation with WAL, indexes, retention trigger.
- Event insertion (single and batch).
- Pending event retrieval with limit.
- Mark events uploaded.
- Crash report insertion and retrieval.
- `TelemetryHandle` with convenience methods (`session_start`, `tool_used`, `api_call`, `error`).
- Background worker thread with batching and flush intervals.
- Uploader with HTTP POST and Bearer token auth.
- Crash panic hook writing JSON to pending directory.
- Crash ingestion on startup.

### What Is Stubbed/Partial

- **Uploader is never created by `TelemetryHandle::new`**: Returns `Option<TelemetryUploader>` as `None` (`lib.rs:56`).
- **No graceful shutdown**: Worker thread loops forever; no way to signal shutdown and flush final batch.
- **No upload retry backoff**: Failed uploads are logged but retried on next interval with no backoff.
- **Crash upload endpoint is hardcoded**: Derived from events endpoint by string manipulation (`trim_end_matches("/events")`).

### Tests

- 4 tests in `store.rs` (insert, batch, mark uploaded, crash roundtrip).
- 1 test in `uploader.rs` (config defaults).
- 1 test in `crash.rs` (serialization).
- Tests use tempfile/NamedTempFile for SQLite.

## Gaps for Production

- **CRITICAL — Uploader not wired up**: `TelemetryHandle::new` always returns `None` for uploader; no background upload ever runs.
  → **Suggestion**: Pass `TelemetryConfig` (or `Option<TelemetryConfig>`) into `TelemetryHandle::new`. If config is present and `enabled` is true, construct the `TelemetryUploader` and spawn the upload worker. Gate this behind a feature flag for users who want to compile without telemetry.
- **CRITICAL — No graceful shutdown**: `run_store_worker` has no shutdown signal; dropping the handle closes the channel but there's no clean stop.
  → **Suggestion**: Add a `shutdown: tokio::sync::Notify` or `mpsc::Sender<WorkerCommand>` to the worker. On `TelemetryHandle::shutdown()`, send a `FlushAndExit` command. The worker flushes the final batch, attempts one last upload, and exits cleanly.
- **SECURITY — API key in plain text**: `TelemetryConfig.api_key` is `Option<String>` with no encryption or keychain integration.
  → **Suggestion**: Store the API key in the OS keychain via `keyring` crate. In config, use `"api_key": "keyring://coderex-telemetry"` as a reference. Never log or serialize the raw key.
- **SECURITY — No certificate pinning**: reqwest client uses default TLS config; no validation of telemetry endpoint identity.
  → **Suggestion**: Pin the telemetry server's certificate or public key hash in the binary. Use `reqwest::ClientBuilder::add_root_certificate()` with a custom CA or implement HPKP-style pinning. Fail closed on mismatch.
- **PRIVACY — No user consent gate**: Claims "privacy-first and opt-out" but no actual UI or API for users to opt out.
  → **Suggestion**: On first run, show a one-time consent dialog: "Help improve CodeRex by sending anonymous usage data? [Yes/No]". Store the choice in `~/.coderex/settings.json` under `telemetry.enabled`. Respect the choice immediately; default to disabled in GDPR jurisdictions.
- **PRIVACY — No PII scrubbing**: Comments claim "sanitized, no PII" but `Error` event takes raw message string with no scrubbing logic.
  → **Suggestion**: Implement a `Scrubber` that regex-replaces: email addresses, IP addresses, file paths containing the username, and API keys. Run it on all `Error` and `ApiCall` events before insertion. Audit the regex list quarterly.
- **Hardcoded defaults**: Endpoint `https://telemetry.coderex.dev/v1/events`, batch 100, interval 300s.
  → **Suggestion**: Move all defaults to a `TelemetryDefaults` struct compiled into the binary. Allow override via `TelemetryConfig` from `settings.json`. Document the override keys in the user manual.
- **No disk space protection**: Crash pending directory can grow unbounded if ingestion fails.
  → **Suggestion**: Add a `max_pending_crash_bytes: u64` limit (default 50MB). On startup, if the pending directory exceeds the limit, delete oldest files (by mtime) until under limit. Log a warning when cleanup occurs.
- **Retention trigger is coarse**: Deletes exactly 100 oldest events when exceeding 10000, but does not check total size on disk.
  → **Suggestion**: Add a second retention policy based on total DB size (e.g., max 100MB). Use `PRAGMA page_count * page_size` to compute size. If exceeded, delete oldest events proportionally. Run both checks daily, not just on insert.
- **No upload compression**: JSON payloads sent uncompressed.
  → **Suggestion**: Compress JSON payloads with `flate2` (gzip) before POSTing. Add `Content-Encoding: gzip` header. Measure CPU vs bandwidth tradeoff; if payload is <1KB, skip compression.
- **No circuit breaker**: Failed uploads retry immediately on next tick; could hammer a failing server.
  → **Suggestion**: Implement a circuit breaker with three states: Closed (normal), Open (skip uploads for 60s after 5 consecutive failures), HalfOpen (try one upload). Track failure count and last failure time per endpoint.
- **No event sampling**: All events recorded; high-volume usage could overwhelm local DB.
  → **Suggestion**: Add a `sample_rate: f64` to `TelemetryConfig` (default 1.0). For each event type, hash a deterministic key (event type + session_id + timestamp bucket). Only record if hash < sample_rate * u64::MAX. Allow per-event-type rates.
- **Worker thread panic handling**: If `flush_batch` panics, the worker thread dies silently; no recovery.
  → **Suggestion**: Wrap the worker loop body in `std::panic::catch_unwind` (if Send). On panic, log the backtrace, mark the store as unhealthy, and respawn the worker thread after a 10s cooldown. Limit respawns to 3 before giving up.
- **Timestamp inconsistency**: Events use `chrono::Utc::now()` at insert time, but `to_db()` generates a new timestamp rather than using the event's original timestamp.
  → **Suggestion**: Store the original `created_at: DateTime<Utc>` in `TelemetryEvent`. Pass it through `to_db()` unchanged. Use `Utc::now()` only when constructing the event, not when serializing.
- **Missing indices on crash_reports**: No index on `crash_reports(uploaded_at)` or `(created_at)`.
  → **Suggestion**: Add `CREATE INDEX idx_crash_uploaded ON crash_reports(uploaded_at)` and `CREATE INDEX idx_crash_created ON crash_reports(created_at)` in the schema migration. This speeds up pending crash queries and retention cleanup.
