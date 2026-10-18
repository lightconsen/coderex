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
- **`TelemetryUploader`** — reqwest client + own DB connection + config + circuit breaker + rate limiter + optional `TelemetryChannels`. Opens a fresh SQLite connection per upload batch for safe concurrent access.
- **`ChannelConfig` / `TelemetryChannels`** (`channels.rs`) — Optional OTLP/HTTP JSON logs and Datadog Logs export channels. `ChannelConfig::from_env` reads the `CODEREX_OTLP_*` / `CODEREX_DATADOG_*` variables; `TelemetryUploader::with_channels` accepts an explicit configuration. See [Channels](#channels).
- **`CircuitBreaker`** — Three-state breaker (Closed/Open/HalfOpen). Opens after 5 consecutive failures, cools down for 60s, then transitions to HalfOpen. Handles *failures*.
- **`RateLimiter`** — Sliding-window limiter for *rejections*: at most 10 upload attempts per 60s, and after the endpoint answers 429/403 uploads are suppressed for 300s. `upload_pending` surfaces those statuses as `TelemetryError::Rejected` so the loop can back off instead of retrying into the limit.

### Key Types (scrubber.rs)

- **`Scrubber`** — Pattern-and-checksum sanitizer. Replaces emails, IPs, home paths, credential shapes, financial identifiers, and long high-entropy tokens with `<EMAIL>`, `<IP>`, `<USER>`, `<API_KEY>`, `<AUTH_TOKEN>`, `<JWT>`, `<PRIVATE_KEY>`, `<CREDIT_CARD>`, `<SSN>`, `<IBAN>`, `<SECRET>` tokens. Applied to `Error.message`, `ApiCall.provider/model`, and `ToolUsed.tool_name` before insertion, and recursively to every uploaded payload.
- **High-entropy detector** (`is_high_entropy_token`, `scrub_high_entropy_tokens`, placeholder `<SECRET>`) — redacts standalone base64url/hex/mixed-alphanumeric runs of 32+ characters. It is deliberately conservative: paths, URLs, Rust symbols, backtrace frames and ordinary identifiers are left alone (see the exact rules below).
- **Financial/numeric detectors** — Luhn-validated payment cards (`<CREDIT_CARD>`), US SSNs in SSA-valid ranges (`<SSN>`), and mod-97-validated IBANs (`<IBAN>`). Each rejects matches embedded in paths, filenames, URLs or `file:line` frames (see below).
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
`tokens_in`, `tokens_out`, `error_type`, `message`, `timestamp`, `panic_info`, `backtrace`,
`redaction_status`, `panic_type`, `thread_name`, `panic_location`, `os`, `version`). The check is
recursive: every key at every level must be known. A payload that
**does not** validate is still sent, but is marked with `"redaction_status": "unredacted"` **inside
the payload itself** — so the mark travels in the stored/uploaded event, not a log line. The payload
is not silently dropped (that would lose telemetry) and not silently sent unmarked (that would claim
a validation that did not happen). `redaction_status` is itself allowlisted so an explicit mark (for
example the crash "structured" mark) is never overwritten with "unredacted".

### What the scrubber removes — and what it does not

The scrubber is **pattern-based plus a few checksum validators**. It removes shapes, not meaning.
Concretely, it replaces:

- email addresses (`<EMAIL>`), IPv4/IPv6 addresses (`<IP>`);
- `/home/<name>` and `/Users/<name>` path segments (`<USER>`) — the rest of the path is kept;
- PEM private-key **blocks** (header through `END`, with a header-only fallback for a truncated
  block), bearer values, JWTs (`eyJ…` with exactly three dot-separated segments), GitHub tokens
  (`ghp_…`, `github_pat_…`), AWS access key IDs (`AKIA` plus 16 alphanumerics), Slack tokens
  (`xox[baprs]-…`), and labelled `sk-` / `api_key` / `token` / `secret` values
  (`<PRIVATE_KEY>`, `<AUTH_TOKEN>`, `<JWT>`, `<API_KEY>`);
- payment card numbers of 13–19 digits that pass the **Luhn** checksum, including space- and
  hyphen-grouped forms (`<CREDIT_CARD>`);
- US SSNs in `AAA-GG-SSSS` form whose fields are in SSA-issued ranges — area not `000`, `666` or
  `9xx`; group not `00`; serial not `0000` (`<SSN>`);
- IBANs of 15–34 alphanumerics that pass the ISO 13616 **mod-97** checksum, with or without
  single-space grouping (`<IBAN>`);
- standalone high-entropy runs of **32 or more** characters drawn from
  `[A-Za-z0-9_+=-]` (`<SECRET>`). A run qualifies when it is all-hex of that length, or when it
  mixes uppercase, lowercase and digits with Shannon entropy of at least 3.0 bits/character.

The numeric detectors are deliberately conservative at the edges too: a card-like run embedded in
a path (`/data/4111111111111111/report`), a filename (`card-4111111111111111.txt`), a URL, or a
`file:line` frame is left alone, as are numbers that fail Luhn, SSNs in never-issued ranges, and
IBAN-shaped strings that fail mod-97.

It does **not** remove:

- ordinary prose or conversation text (a sentence accidentally embedded in a panic message stays
  readable unless it contains one of the patterns above);
- file paths, source locations (`file.rs:12`), or Rust symbols (`module::function`,
  `Type::method`, mangled `_ZN…`/`_R…` names);
- URLs — `/`, `.`, `:` split candidate runs, and a run adjacent to `/`, `\`, `.`, `@`, `~` or `::`
  is treated as part of a structured value and left alone;
- `snake_case`, `kebab-case`, `CamelCase`, or `CONSTANT` identifiers, and short/low-entropy runs.

**No semantic understanding is claimed.** The scrubber cannot tell a conversation sentence from a
message, and it cannot prove that a payload is free of secrets. Free-form crash text is therefore
not sent by default at all (see below); when `CODEREX_CRASH_VERBOSE` is enabled it is sent only
after these patterns run, and it is explicitly marked as pattern-scrubbed rather than clean.

### Key Types (crash.rs)

- **`CrashReport`** — Serializable crash data: `panic_info`, optional raw `backtrace`, timestamp,
  panic metadata (`panic_location`, `thread_name`, `panic_type`), the structured classification
  (`panic_template`, `panic_fields`, `panic_message_len`, `panic_message_sha256`) and the
  symbol-only backtrace (`backtrace_symbols`, frame count, symbol length/hash). Older pending files
  deserialize with all new fields defaulted.
- **`PanicTemplate` / `PanicClassification` / `classify_panic_message`** — the panic taxonomy. A
  message is reduced to a fixed template (`unwrap_err`, `unwrap_none`, `expect_err`, `expect_none`,
  `index_out_of_bounds`, `slice_range`, `capacity_overflow`, `arithmetic_overflow`,
  `assertion_failed`, `explicit_panic`, `poisoned_lock`, or `unknown`) plus at most a few safe
  numeric/identifier fields. The raw message is never retained by the classifier; its byte length
  and SHA-256 are.
- **`BacktraceSymbols` / `symbolize_backtrace`** — frame symbols only. `at <path>` continuation
  lines, `note:` lines and `<unknown>` frames are dropped; a symbol containing `/`, `\`, `@` or `~`
  is dropped rather than truncated. At most **`MAX_BACKTRACE_FRAMES` = 24** frames are kept, and
  `symbols_len` / `symbols_sha256` describe exactly the kept frames joined by `\n`.
- **`CrashReporter`** — Panic hook + pending directory ingestion with age and disk-space enforcement.
  `strict` is resolved from `CODEREX_HIPAA` and `verbose` from `CODEREX_CRASH_VERBOSE` at
  construction (overridable with `with_strict` / `with_verbose` for tests).
- **`crash_verbose()` / `crash_verbose_with()`** — the `CODEREX_CRASH_VERBOSE` trigger (non-empty
  and not `0`/`false`/`no`).
- **`preview_crash_payload` / `preview_crash_payload_with` /
  `TelemetryUploader::preview_crash_payload`** — return the **exact JSON** that would be uploaded
  for a `PendingCrash` row. The free functions resolve the two environment variables at call time;
  the method uses the uploader's own resolved modes and scrubber. Verbose text is already scrubbed
  in the preview, so the user sees the wire form rather than the raw local copy.

### Crash reports: safe by default

A panic message is `format!("{info}")` of whatever the panicking code carried — potentially
conversation text. Pattern scrubbing cannot make that safe, so **free-form text is never uploaded
by default**:

- **Panic hook (capture at the source):** the message is classified *in memory* into a
  `PanicTemplate` plus safe parsed fields, and its length and SHA-256 are recorded. The default
  pending file contains no message text. A backtrace is captured and immediately reduced to frame
  symbols; only the symbols are kept. Under `CODEREX_HIPAA` no backtrace is captured at all and no
  symbols are kept.
- **Ingestion:** the structured summary and the classification columns are stored. A legacy pending
  file or database row with raw text is classified on the fly and reduced to a summary; the message
  is never stored outside verbose mode. A legacy raw backtrace is symbolized on ingestion.
- **Upload:** `build_crash_payload` sends `panic_type`, `thread_name`, `panic_location`, `os`,
  `version`, `timestamp`, `panic_template`, `panic_message_len`, `panic_message_sha256`, the safe
  parsed fields, and — when available — `backtrace_frames`, `backtrace_frame_count`,
  `backtrace_symbols_len` and `backtrace_symbols_sha256`. The location has its leading root
  removed and its home user redacted, so no absolute path is uploaded. There is no `panic_info`
  field, no raw `backtrace` field and no `message` field.

`CODEREX_CRASH_VERBOSE` (non-empty and not `0`/`false`/`no`) opts into a **raw local copy**:

- **Panic hook:** the free-form message and raw backtrace are written to the pending file, which is
  mode **`0600`** on Unix. The message is still classified and the backtrace is still symbolized.
- **Ingestion:** the raw text is kept in the local SQLite store (the database and its SQLite
  sidecars are also set to `0600` on Unix). This is the user-only copy for diagnosis. The raw
  backtrace is **never** uploaded.
- **Retention:** pending files are deleted after **7 days** (`DEFAULT_PENDING_MAX_AGE_SECS`), and
  stored crash rows after **30 days** (`DEFAULT_CRASH_RETENTION_DAYS`). The pending directory is
  additionally capped at 50 MB (oldest first).
- **Upload:** `panic_info` is re-scrubbed at upload (pattern set, checksum detectors and
  high-entropy detector) and sent with the taxonomy and symbol-only frames, marked
  `"redaction_status": "pattern"`. The marker never claims the text is clean, because the scrubber
  has no semantic understanding.
- **`CODEREX_HIPAA` always wins:** even with verbose enabled, strict mode sends structured-only —
  no text and no symbols.

`redaction_status` markers are payload fields, so they travel with the report rather than living in
a log line. The tri-state deliberately has no `clean` value:

| Value | Meaning |
|---|---|
| `structured` | Taxonomy/metadata only; no free-form text and no backtrace frames. |
| `symbolized` | Adds `backtrace_frames` (symbols only, no paths); still no free-form text. |
| `pattern` | Adds `panic_info` after pattern scrubbing. Not a claim of safety. |
| `unredacted` | Strict-mode allowlist validation failed for a non-crash payload (event path). |

The keys are selected by the marker: `structured` and `symbolized` payloads contain neither
`panic_info` nor `message`, and the raw `backtrace` key is never produced in any mode.

### Previewing a crash without uploading

`coderex telemetry preview-crash` reads one pending crash file, applies the
same ingestion modes a real startup would apply, and prints the exact JSON the
uploader would send:

```bash
# The newest file in ~/.coderex/crashes/pending
coderex telemetry preview-crash

# A specific pending file
coderex telemetry preview-crash ~/.coderex/crashes/pending/crash_1729.json
```

The command does **not** ingest, delete, or upload anything. Pending files are
written by the panic hook as `crash_<millis>.json`; the default selection is
the newest `*.json` by modification time. `CLAUDE_CONFIG_DIR` (the config-home
override) is honored, as it is for the uploader.

Tuning the modes works the same way it does for a real upload:

```bash
CODEREX_CRASH_VERBOSE=1 coderex telemetry preview-crash   # adds scrubbed panic_info
CODEREX_HIPAA=1 coderex telemetry preview-crash           # structured only
```

`CODEREX_HIPAA` wins over `CODEREX_CRASH_VERBOSE`. Verbose output only appears
when the pending file actually captured the raw local copy (the panic hook
wrote it while verbose was enabled); previewing a structured file with verbose
on does not invent text. The printer uses the same
`preview_crash_payload` path as the uploader, so the keys and the
`redaction_status` marker match what a real upload would carry.

`CrashReporter::pending_crash_from_report` exposes the conversion for callers
that want to build the same row shape in-process.

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
- **PII scrubbing**: `Scrubber` regex-sanitizes `Error` and `ApiCall` events before insertion, including high-entropy secret tokens (`<SECRET>`).
- **Worker panic recovery**: `spawn_worker_with_recovery()` uses `catch_unwind` + fresh connection respawn (max 3 attempts, 10s cooldown).
- **Certificate pinning**: Optional `pinned_cert_pem` in `TelemetryConfig` adds a custom root certificate to the reqwest client.
- **Settings.json integration**: `TelemetrySubConfig` nested struct (schema v3) loads user overrides from `~/.coderex/settings.json`.

### Public API Surface

- `TelemetryHandle`, `TelemetryEvent`, `TelemetryStore`, `TelemetryUploader`, `TelemetryConfig`, `CrashReporter`, `CrashReport`, `PendingEvent`, `PendingCrash`, `Scrubber`, `HIPAA_ENV`, `hipaa_enabled`, `CRASH_VERBOSE_ENV`, `crash_verbose`, `crash_verbose_with`, `PanicTemplate`, `PanicClassification`, `classify_panic_message`, `BacktraceSymbols`, `symbolize_backtrace`, `symbolize_backtrace_limited`, `MAX_BACKTRACE_FRAMES`, `DEFAULT_PENDING_MAX_AGE_SECS`, `DEFAULT_CRASH_RETENTION_DAYS`, `preview_crash_payload`, `preview_crash_payload_with`, `ChannelConfig`, `TelemetryChannels`, `OTLP_ENDPOINT_ENV`, `OTLP_HEADERS_ENV`, `DATADOG_API_KEY_ENV`, `DD_API_KEY_ENV`, `DATADOG_SITE_ENV`, `DATADOG_ENDPOINT_ENV`, `DEFAULT_DATADOG_SITE`. `CrashReporter::pending_crash_from_report` builds the upload-row shape from a pending file (used by `coderex telemetry preview-crash`).

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
- **PII scrubbing** via `Scrubber` (emails, IPs, home paths, AWS/GitHub/Slack tokens, bearer values, JWTs, PEM private-key blocks, Luhn-valid card numbers, SSA-valid SSNs, mod-97-valid IBANs, and 32+ character high-entropy tokens as `<SECRET>`) applied to `Error`, `ApiCall`, and `ToolUsed` events, plus an `EXCLUDED_FIELDS` exclusion set applied recursively to payloads at upload time. Detection is pattern/checksum-based and deliberately conservative: paths, URLs, symbols, frames and ordinary prose are not touched, and card-like runs in paths or filenames survive.
- **Crash reports are structured/symbolized by default**: the panic hook classifies the message into a fixed taxonomy (never storing the raw text), records its length and SHA-256, and reduces the backtrace to at most 24 symbol frames with no paths. Upload sends the taxonomy, the safe parsed fields (`panic_index`, `panic_container_len`, `panic_range_start`, `panic_range_end`, `panic_slice_len`, `panic_operation`, `panic_error_type`) and the symbol frame list/hash. `CODEREX_CRASH_VERBOSE` opts into a raw local copy (`0600`) whose text is scrubbed at upload and marked `"pattern"`; the raw backtrace is never uploaded. `CODEREX_HIPAA` always forces structured-only (no symbols either). Legacy raw rows are classified on the fly and never upload free-form text unless verbose is enabled.
- **Model-name redaction**: the model-field key regex (including `*_models` / `*Models`), the always-model keys (`chain_entry`, `fallbackFrom`, `shortName`), the `repository_model` exclusion, the never-replaced placeholder set, and the `"confidential"` constant. Applied recursively to every uploaded payload by `Scrubber::scrub_json`.
- **Strict (confidentiality) mode**: `CODEREX_HIPAA` turns on a recursive field-allowlist check; a payload that fails it is uploaded marked `"redaction_status": "unredacted"` in the payload itself.
- **Worker panic recovery** with `catch_unwind`, fresh DB connection per respawn, 3-attempt limit, 10s cooldown.
- **Certificate pinning** via `TelemetryConfig.pinned_cert_pem` + `add_root_certificate()`.
- **Opt-in consent gate**: telemetry is off until the user consents. `Settings::defaults()` leaves `telemetry_enabled` as `None` ("undecided"), so the first-run prompt in `StartupOrchestrator::phase_load_settings` is actually reachable; an undecided value is treated as *disabled*. The choice is persisted to `settings.json`.
- **Rate limiting / suppression**: `RateLimiter` caps uploads at 10/minute and suppresses them for 300s after an HTTP 429/403.
- **Settings.json integration** via `TelemetrySubConfig` nested struct (schema v3), including the optional OTLP/Datadog channel fields.
- **OTLP/HTTP JSON logs channel** (`channels.rs`): endpoint derivation (base URL or full `/v1/logs`), extra headers, resource/scope metadata, per-event log records with severity, attributes and nanosecond timestamps.
- **Datadog Logs channel** (`channels.rs`): site-derived or explicit intake URL, `DD-API-KEY` auth, per-event log entries with `ddsource`/`service`/`ddtags`/`status`, millisecond timestamps, and no hostname.
- **Channel consent gate**: channels are built only for an enabled uploader; `upload_pending()` is a no-op when disabled and `send_events()` refuses to send when its own switch is off.

### Channels

Events are always uploaded to the first-party HTTP JSON endpoint
(`uploader.rs`). Two optional additional channels can carry the same events:

1. **OTLP/HTTP JSON logs** (`channels.rs`) — plain `POST <endpoint>/v1/logs`
   with an `ExportLogsServiceRequest` body. No OpenTelemetry SDK is used; the
   request is built directly with `serde_json` + `reqwest`.
2. **Datadog Logs intake** (`channels.rs`) — `POST` to
   `https://http-intake.logs.<site>/api/v2/logs` with the standard
   `DD-API-KEY` header and a JSON array of log entries.

Both consume the *same* scrubbed event payload as the first-party uploader
(`Scrubber::scrub_json` runs again on the way out). Only usage events are sent;
crash reports stay on the first-party channel.

#### Consent

Channels are wired to the existing telemetry opt-in:

- `TelemetryUploader::new` builds a channel set only when
  `TelemetryConfig::enabled` is true.
- `upload_pending()` returns early when disabled — no first-party request, no
  channel request, and no row marked uploaded.
- `TelemetryChannels::send_events()` independently refuses to send when its
  own `ChannelConfig::enabled` is false.

`TelemetryHandle::new` still constructs the uploader only when the CLI passes an
enabled configuration, which happens only after `settings.telemetry_enabled` is
`Some(true)`. An undecided (`None`) or false value means nothing is sent.

#### Configuration

Channels are configured from the environment when the uploader is constructed:

| Variable | Meaning |
|---|---|
| `CODEREX_OTLP_ENDPOINT` | OTLP/HTTP base URL (e.g. `http://localhost:4318`); `/v1/logs` is appended unless already present. |
| `CODEREX_OTLP_HEADERS` | Optional comma-separated `key=value` request headers (for collector auth). |
| `CODEREX_DATADOG_API_KEY` | Datadog API key. Falls back to `DD_API_KEY`. |
| `CODEREX_DATADOG_SITE` | Datadog site; default `datadoghq.com` (also accepts an `http-intake.logs.*` host). |
| `CODEREX_DATADOG_ENDPOINT` | Full intake URL, overriding the site-derived endpoint. |

`settings.json` can carry the same values under `telemetry` (`otlp_endpoint`,
`otlp_headers`, `datadog_api_key`, `datadog_site`, `datadog_endpoint`); a caller
that holds those values can pass them explicitly with
`TelemetryUploader::with_channels(ChannelConfig)`. The API key is never rendered
in `Debug` output.

#### Wire mapping

Every `PendingEvent` becomes one OTLP log record / one Datadog log entry:

| Event field | OTLP | Datadog |
|---|---|---|
| `created_at` | `timeUnixNano` / `observedTimeUnixNano` (string) | `timestamp` (milliseconds) |
| `event_type` | `body.stringValue` and the `event.type` attribute | `ddtags` (`event_type:…`) |
| payload fields | one `KeyValue` attribute each (strings, booleans, numbers); objects/arrays become JSON text | inside `message`, the scrubbed payload JSON |
| `error` event | severity 17 / `ERROR` | `status: "error"` |
| other events | severity 9 / `INFO` | `status: "info"` |
| — | resource `service.name = coderex`, SDK name/language | `ddsource`/`service = coderex` |

No hostname is sent to Datadog. Channel delivery is best-effort: the
first-party upload runs and marks the rows first, and a channel failure is
logged rather than retried into a duplicate first-party upload. When one
channel fails, the other is still attempted.

### What Is Stubbed/Partial

None. Model-field redaction and the strict (confidentiality) mode are both
implemented; the only deliberate difference from the reference is that the
strict trigger is the `CODEREX_HIPAA` environment variable rather than an
environment variable *and* a settings key (the settings layer is not reachable
from the telemetry crate).

### Tests

- 8 tests in `store.rs` (insert, batch, mark uploaded, crash roundtrip, structured-record roundtrip, old-schema migration, crash retention, size retention).
- 36 tests in `uploader.rs` (config defaults, crash endpoint explicit/derived, circuit breaker, gzip encoding/header, rate limiter window/suppression, rejection statuses, HIPAA trigger parsing, strict-mode payload marking, structured/symbolized/pattern crash payloads, unknown-message length/hash only, no absolute paths, preview API, HIPAA precedence, legacy rows, tampered-row field validation, disabled uploader sends nothing and keeps rows pending, enabled uploader fans events out to channels).
- 16 tests in `channels.rs` (OTLP endpoint derivation, Datadog site/explicit endpoint/API-key requirements, env parsing, redacted `Debug`, disabled-set construction, OTLP log-record mapping and scrubbing, Datadog entry mapping and scrubbing, and mock-server delivery to both channels including partial failure).
- 22 tests in `crash.rs` (taxonomy for every known template, safe-field parsing, unknown length/hash, symbolization/top-N/path rejection, default structured+symbolized ingestion, verbose raw local copy, strict-over-verbose precedence, legacy raw-file handling, `CODEREX_CRASH_VERBOSE` parsing, `0600` mode, TTL prune, serialization).
- 49 tests in `scrubber.rs` (email, IPv4, home path, API key, GitHub/AWS/Slack/bearer/JWT/private-key, Luhn card/SSN/IBAN, event scrubbing, exclusion set, nested JSON scrubbing, model-key regex, model redaction, placeholder preservation, `repository_model` exclusion, strict allowlist/marker, high-entropy true positives, and path/symbol/URL/prose/numeric false positives).
- Tests use tempfile/NamedTempFile for SQLite.

M1 verification: `cargo test -p coderex-telemetry --offline` → **131 passed / 0 failed**;
`cargo test -p coderex-config --offline` → **90 unit + 40 integration passed / 0 failed**
(including the macOS plist carrier). `cargo test -p coderex-cli --offline -- --test-threads=1`
→ all test binaries passed (the two PATH-sensitive tests flake only when run in
parallel).

## Gaps for Production

None.
