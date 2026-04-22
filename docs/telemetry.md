# telemetry Module Inspection

## Goal

Privacy-first telemetry collection, local storage, and batch upload. Stores events in SQLite locally, uploads in batches when online, and captures crash reports via panic hooks.

## Design

### Key Types (lib.rs)

- **`TelemetryHandle`** — `mpsc::Sender` wrapper for thread-safe event recording. Supports deterministic event sampling (`sample_rate`).
- **`TelemetryError`** — Error enum: Database, Io, Serialization, Upload.
- **`WorkerCommand`** — Internal enum: `Event(TelemetryEvent)` or `FlushAndExit(ack)`.

### Key Types (store.rs)

- **`TelemetryStore`** — SQLite connection wrapper with schema management, size-based retention, and DB size queries.
- **`TelemetryEvent`** — Enum: SessionStart, ToolUsed, ApiCall, Error (with chrono timestamps).
- **`PendingEvent` / `PendingCrash`** — Rows pending upload.

### Key Types (uploader.rs)

- **`TelemetryConfig`** — Endpoint, optional explicit crash endpoint, API key, batch size (default 100), interval (default 300s), enabled flag, sample rate (default 1.0), optional pinned root certificate PEM for TLS pinning.
- **`TelemetryUploader`** — reqwest client + own DB connection + config + circuit breaker. Opens a fresh SQLite connection per upload batch for safe concurrent access.
- **`CircuitBreaker`** — Three-state breaker (Closed/Open/HalfOpen). Opens after 5 consecutive failures, cools down for 60s, then transitions to HalfOpen.

### Key Types (scrubber.rs)

- **`Scrubber`** — Regex-based PII sanitizer. Replaces emails, IPs, home paths, and API keys with `<EMAIL>`, `<IP>`, `<USER>`, `<API_KEY>` tokens. Applied to `Error` and `ApiCall` events before insertion.

### Key Types (crash.rs)

- **`CrashReport`** — Serializable crash data.
- **`CrashReporter`** — Panic hook + pending directory ingestion with disk-space enforcement.

### Architecture Patterns

- **Producer-consumer**: `TelemetryHandle` (sender) → worker thread (receiver) → SQLite.
- **Batching**: 32-event batches or 30-second flush intervals.
- **WAL mode SQLite** for concurrent reads.
- **Retention triggers**: count-based (delete 100 oldest when >10000 rows) + size-based (`prune_to_size()` via `PRAGMA page_count * page_size`).
- **Crash reports written to filesystem** during panic (because SQLite is not Sync), then ingested on next startup with a 50MB size limit.
- **Upload compression**: JSON payloads gzip-compressed with `flate2` (skipped if <1KB).
- **Circuit breaker**: Prevents hammering a failing endpoint with exponential backoff via Open/HalfOpen states.
- **Event sampling**: Deterministic hash-based sampling on the serialized event JSON. Same event always yields the same decision.
- **Own-connection uploader**: `TelemetryUploader` opens a fresh `TelemetryStore` connection per batch instead of sharing the worker's connection, avoiding Sync issues.
- **PII scrubbing**: `Scrubber` regex-sanitizes `Error` and `ApiCall` events before insertion.
- **Worker panic recovery**: `spawn_worker_with_recovery()` uses `catch_unwind` + fresh connection respawn (max 3 attempts, 10s cooldown).
- **Certificate pinning**: Optional `pinned_cert_pem` in `TelemetryConfig` adds a custom root certificate to the reqwest client.
- **Settings.json integration**: `TelemetrySubConfig` nested struct (schema v3) loads user overrides from `~/.coderex/settings.json`.

### Public API Surface

- `TelemetryHandle`, `TelemetryEvent`, `TelemetryStore`, `TelemetryUploader`, `TelemetryConfig`, `CrashReporter`, `CrashReport`, `PendingEvent`, `PendingCrash`, `Scrubber`.

## Current Status

### What Works

- SQLite schema creation with WAL, indexes, dual retention triggers.
- Event insertion (single and batch) with original timestamp preserved.
- Pending event retrieval with limit.
- Mark events uploaded.
- Crash report insertion and retrieval.
- `TelemetryHandle` with convenience methods (`session_start`, `tool_used`, `api_call`, `error`).
- Background worker thread with batching and flush intervals.
- **Graceful shutdown**: `TelemetryHandle::shutdown()` sends `FlushAndExit`, worker flushes final batch and acks.
- **Uploader wired up**: `TelemetryHandle::new` accepts `Option<TelemetryConfig>`; constructs `TelemetryUploader` when enabled.
- **Circuit breaker**: 5-failure threshold, 60s cooldown, HalfOpen retry.
- **Gzip upload compression**: `flate2` with 1KB skip threshold.
- **Crash panic hook** writing JSON to pending directory.
- **Crash ingestion** on startup with 50MB disk-space enforcement (oldest files deleted first).
- **Event sampling** via `TelemetryConfig.sample_rate` (0.0–1.0).
- **Size-based retention** via `TelemetryStore::prune_to_size()`.
- **Explicit crash endpoint** override (`TelemetryConfig.crash_endpoint`) or derived from events endpoint.
- **DB size queries** via `PRAGMA page_count * page_size`.
- **PII scrubbing** via `Scrubber` (emails, IPs, paths, API keys) applied to `Error` and `ApiCall` events.
- **Worker panic recovery** with `catch_unwind`, fresh DB connection per respawn, 3-attempt limit, 10s cooldown.
- **Certificate pinning** via `TelemetryConfig.pinned_cert_pem` + `add_root_certificate()`.
- **User consent gate** on first run, persisted to `settings.json`.
- **Settings.json integration** via `TelemetrySubConfig` nested struct (schema v3).

### What Is Stubbed/Partial

None — all previously stubbed items are now implemented.

### Tests

- 4 tests in `store.rs` (insert, batch, mark uploaded, crash roundtrip).
- 6 tests in `uploader.rs` (config defaults, crash endpoint explicit/derived, circuit breaker, compression).
- 1 test in `crash.rs` (serialization).
- 5 tests in `scrubber.rs` (email, IPv4, home path, API key, event scrubbing).
- Tests use tempfile/NamedTempFile for SQLite.

## Gaps for Production

None — all identified production gaps are implemented.
