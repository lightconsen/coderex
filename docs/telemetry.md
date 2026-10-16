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
- **`TelemetryUploader`** — reqwest client + own DB connection + config + circuit breaker + rate limiter. Opens a fresh SQLite connection per upload batch for safe concurrent access.
- **`CircuitBreaker`** — Three-state breaker (Closed/Open/HalfOpen). Opens after 5 consecutive failures, cools down for 60s, then transitions to HalfOpen. Handles *failures*.
- **`RateLimiter`** — Sliding-window limiter for *rejections*: at most 10 upload attempts per 60s, and after the endpoint answers 429/403 uploads are suppressed for 300s. `upload_pending` surfaces those statuses as `TelemetryError::Rejected` so the loop can back off instead of retrying into the limit.

### Key Types (scrubber.rs)

- **`Scrubber`** — Regex-based PII sanitizer. Replaces emails, IPs, home paths, and credential shapes with `<EMAIL>`, `<IP>`, `<USER>`, `<API_KEY>`, `<AUTH_TOKEN>`, `<JWT>`, `<PRIVATE_KEY>` tokens. Applied to `Error.message`, `ApiCall.provider/model`, and `ToolUsed.tool_name` before insertion.
- **`EXCLUDED_FIELDS`** — Field names (`api_key`, `authorization`, `password`, `secret`, `token`, `cookie`, `set-cookie`, `apikey`) that `Scrubber::scrub_json` **removes** from a payload entirely, recursively, as a second layer at upload time.
- **Model-name redaction** (`is_model_key`, `redact_model_fields`, `CONFIDENTIAL_MODEL`) — a dedicated model-field key regex plus an exclusion set, run recursively on every payload by `Scrubber::scrub_json` (see below).
- **Strict mode** (`strictly_allowlisted`, `mark_unredacted_if_unallowlisted`, `STRICT_ALLOWED_FIELDS`) — validates a payload against a field allowlist and marks anything it cannot validate (see below).

### Model-name redaction

A JSON object key is a **model field** when either:

- it is one of the always-model keys `chain_entry`, `fallbackFrom`, `shortName`; or
- it is **not** `repository_model` (the explicit exclusion) **and** matches the model-key regex
  `^(?:model|model_id|[A-Za-z0-9_]*_models?|[A-Za-z0-9_]*[a-z0-9]Models?)$`
  — i.e. `model`, `model_id`, `*_model`, `*_models`, `*Model`, `*Models`.

The exclusion is applied before the regex because `repository_model` matches the regex
(`repository` + `_model`); it names a repository relationship, not a model, and is left verbatim.

A model field's value is replaced with the constant `"confidential"`. Two kinds of value are
**not** replaced:

- the placeholders `inherit`, `default`, `none`, `unknown`, `nonconforming` — they name no model,
  so keeping them is honest; and
- nothing else: a concrete model id **and** an unavailable value (empty string, or a non-string)
  both become `"confidential"`.

**The confidentiality rule.** The reference substitutes `"confidential"` only for models on a
"needs confidentiality" list (and for unavailable models). CodeRex has one first-party channel and
no cloud model catalogue, so it cannot know which names are sensitive. It therefore treats **every
concrete model name as confidential** and redacts all of them, keeping only the non-identifying
placeholders. This is stricter than the reference's default and is the honest choice for a client
that cannot enumerate the sensitive set.

### Strict mode (confidentiality trigger)

Strict mode is turned on by the environment variable **`CODEREX_HIPAA`** — present with any value
other than `0`/`false`/`no`/empty (`hipaa_enabled`). It mirrors the reference's `hipaa`
setting/lookup trigger. The reference also carries a settings key; CodeRex's settings layer is not
reachable from this crate, so the environment variable is the whole trigger.

When on, a payload must validate against `STRICT_ALLOWED_FIELDS` — the union of the event and crash
schema fields (`type`, `session_id`, `tool_name`, `success`, `duration_ms`, `provider`, `model`,
`tokens_in`, `tokens_out`, `error_type`, `message`, `timestamp`, `panic_info`, `backtrace`). The
check is recursive: every key at every level must be known. A payload that **does not** validate is
still sent, but is marked with `"redaction_status": "unredacted"` **inside the payload itself** —
so the mark travels in the stored/uploaded event, not a log line. The payload is not silently
dropped (that would lose telemetry) and not silently sent unmarked (that would claim a validation
that did not happen).

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

- `TelemetryHandle`, `TelemetryEvent`, `TelemetryStore`, `TelemetryUploader`, `TelemetryConfig`, `CrashReporter`, `CrashReport`, `PendingEvent`, `PendingCrash`, `Scrubber`, `HIPAA_ENV`, `hipaa_enabled`.

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
- **PII scrubbing** via `Scrubber` (emails, IPs, paths, API keys, bearer tokens, JWTs, PEM private keys) applied to `Error`, `ApiCall`, and `ToolUsed` events, plus an `EXCLUDED_FIELDS` exclusion set applied recursively to payloads at upload time.
- **Model-name redaction**: the model-field key regex (including `*_models` / `*Models`), the always-model keys (`chain_entry`, `fallbackFrom`, `shortName`), the `repository_model` exclusion, the never-replaced placeholder set, and the `"confidential"` constant. Applied recursively to every uploaded payload by `Scrubber::scrub_json`.
- **Strict (confidentiality) mode**: `CODEREX_HIPAA` turns on a recursive field-allowlist check; a payload that fails it is uploaded marked `"redaction_status": "unredacted"` in the payload itself.
- **Worker panic recovery** with `catch_unwind`, fresh DB connection per respawn, 3-attempt limit, 10s cooldown.
- **Certificate pinning** via `TelemetryConfig.pinned_cert_pem` + `add_root_certificate()`.
- **Opt-in consent gate**: telemetry is off until the user consents. `Settings::defaults()` leaves `telemetry_enabled` as `None` ("undecided"), so the first-run prompt in `StartupOrchestrator::phase_load_settings` is actually reachable; an undecided value is treated as *disabled*. The choice is persisted to `settings.json`.
- **Rate limiting / suppression**: `RateLimiter` caps uploads at 10/minute and suppresses them for 300s after an HTTP 429/403.
- **Settings.json integration** via `TelemetrySubConfig` nested struct (schema v3).

### Channels

CodeRex has one channel: a first-party HTTP JSON uploader (`uploader.rs`). Claude
has three (first-party OTel Logs, user-configured OTel, Datadog — first-party
only). The extra channels are deliberately **not** copied (⚪): each requires a
heavy SDK dependency (OpenTelemetry / Datadog), which is out of scope for this
work, and their wire formats are not the CodeRex event schema. The single
first-party channel is the only one with a real consumer here.

### What Is Stubbed/Partial

None. Model-field redaction and the strict (confidentiality) mode are both
implemented; the only deliberate difference from the reference is that the
strict trigger is the `CODEREX_HIPAA` environment variable rather than an
environment variable *and* a settings key (the settings layer is not reachable
from the telemetry crate).

### Tests

- 5 tests in `store.rs` (insert, batch, mark uploaded, crash roundtrip, size retention).
- 20 tests in `uploader.rs` (config defaults, crash endpoint explicit/derived, circuit breaker, gzip encoding/header, rate limiter window/suppression, rejection statuses, HIPAA trigger parsing, strict-mode payload marking).
- 2 tests in `crash.rs` (serialization, ingest scrubbing).
- 22 tests in `scrubber.rs` (email, IPv4, home path, API key, GitHub/AWS/bearer/JWT/private-key, event scrubbing, exclusion set, nested JSON scrubbing, model-key regex, model redaction, placeholder preservation, `repository_model` exclusion, strict allowlist/marker).
- Tests use tempfile/NamedTempFile for SQLite.

## Gaps for Production

None.
