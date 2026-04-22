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

- **`TelemetryConfig`** — Endpoint, optional explicit crash endpoint, API key, batch size (default 100), interval (default 300s), enabled flag, sample rate (default 1.0).
- **`TelemetryUploader`** — reqwest client + own DB connection + config + circuit breaker. Opens a fresh SQLite connection per upload batch for safe concurrent access.
- **`CircuitBreaker`** — Three-state breaker (Closed/Open/HalfOpen). Opens after 5 consecutive failures, cools down for 60s, then transitions to HalfOpen.

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

### Public API Surface

- `TelemetryHandle`, `TelemetryEvent`, `TelemetryStore`, `TelemetryUploader`, `TelemetryConfig`, `CrashReporter`, `CrashReport`, `PendingEvent`, `PendingCrash`.

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

### What Is Stubbed/Partial

None — all previously stubbed items are now implemented.

### Tests

- 4 tests in `store.rs` (insert, batch, mark uploaded, crash roundtrip).
- 6 tests in `uploader.rs` (config defaults, crash endpoint explicit/derived, circuit breaker, compression).
- 1 test in `crash.rs` (serialization).
- Tests use tempfile/NamedTempFile for SQLite.

## Gaps for Production

### Implemented

| Gap | Implementation |
|-----|----------------|
| Uploader not wired up | `TelemetryHandle::new(store, config)` accepts `Option<TelemetryConfig>`. If present and `enabled`, constructs `TelemetryUploader` with the store's DB path. |
| No graceful shutdown | `TelemetryHandle::shutdown()` sends `WorkerCommand::FlushAndExit(ack_tx)`. Worker flushes batch, sends ack, exits. 5-second timeout on caller side. |
| No upload retry backoff | `CircuitBreaker` in `uploader.rs` tracks consecutive failures. Opens after 5 failures → skips uploads for 60s → transitions to HalfOpen → tries one upload → Closed on success, Open on failure. |
| Crash endpoint hardcoded | `TelemetryConfig` has `crash_endpoint: Option<String>`. `resolve_crash_endpoint()` uses explicit value or derives from events endpoint. |
| Timestamp inconsistency | `TelemetryEvent::to_db()` extracts the original `timestamp` field from each variant instead of generating `Utc::now()`. |
| Missing crash_report indices | Schema creates `idx_crash_uploaded ON crash_reports(uploaded_at)` and `idx_crash_created ON crash_reports(created_at)`. |
| No disk space protection | `CrashReporter::enforce_size_limit()` computes total `.json` size, deletes oldest by mtime until under `max_pending_bytes` (default 50MB). Called in `ingest_pending()`. |
| No upload compression | `compress_json()` uses `flate2::GzEncoder`. Payloads <1KB skip compression. `Content-Encoding: gzip` header added. |
| No circuit breaker | `CircuitBreaker` with `AtomicUsize` state, `AtomicUsize` failure count, `AtomicU64` last failure timestamp. |
| No event sampling | `TelemetryHandle::record()` hashes serialized JSON with `DefaultHasher`. Skips if `hash / u64::MAX >= sample_rate`. |
| Retention trigger is coarse | Added `TelemetryStore::prune_to_size(max_bytes)` using `PRAGMA page_count * page_size`. Deletes oldest 100 events iteratively until under limit. |

### Remaining

- **SECURITY — API key in plain text**: `TelemetryConfig.api_key` is `Option<String>` with no encryption or keychain integration.
  - **Suggestion**: Store the API key in the OS keychain via `keyring` crate. In config, use `"api_key": "keyring://coderex-telemetry"` as a reference. Never log or serialize the raw key.
- **SECURITY — No certificate pinning**: reqwest client uses default TLS config; no validation of telemetry endpoint identity.
  - **Suggestion**: Pin the telemetry server's certificate or public key hash in the binary. Use `reqwest::ClientBuilder::add_root_certificate()` with a custom CA or implement HPKP-style pinning. Fail closed on mismatch.
- **PRIVACY — No user consent gate**: Claims "privacy-first and opt-out" but no actual UI or API for users to opt out.
  - **Suggestion**: On first run, show a one-time consent dialog: "Help improve CodeRex by sending anonymous usage data? [Yes/No]". Store the choice in `~/.coderex/settings.json` under `telemetry.enabled`. Respect the choice immediately; default to disabled in GDPR jurisdictions.
- **PRIVACY — No PII scrubbing**: Comments claim "sanitized, no PII" but `Error` event takes raw message string with no scrubbing logic.
  - **Suggestion**: Implement a `Scrubber` that regex-replaces: email addresses, IP addresses, file paths containing the username, and API keys. Run it on all `Error` and `ApiCall` events before insertion. Audit the regex list quarterly.
- **Worker thread panic handling**: If `flush_batch` panics, the worker thread dies silently; no recovery.
  - **Suggestion**: Wrap the worker loop body in `std::panic::catch_unwind` (if Send). On panic, log the backtrace, mark the store as unhealthy, and respawn the worker thread after a 10s cooldown. Limit respawns to 3 before giving up.
- **Hardcoded defaults partially addressed**: Endpoint, batch size, and interval are configurable via `TelemetryConfig`, but there is no `settings.json` integration to load user overrides at startup.
  - **Suggestion**: Add a `TelemetryDefaults` struct and load overrides from `~/.coderex/settings.json` under a `telemetry` key.
