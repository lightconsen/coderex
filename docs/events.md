# events Module Inspection

## What this crate is (and is not)

`coderex-events` (`crates/events`) is an **in-process broadcast event bus**,
not an external protocol and not a client SDK. It wraps a
`tokio::sync::broadcast` channel (capacity 4096) that fans a single session's
events out to subscribers *inside the same process*, and carries the
bookkeeping that belongs to a session: permission-request tracking, user
elicitation, and JSON persistence of that state.

It never opens a socket. A reader looking for the structured I/O that an
out-of-process client speaks should look at the **WebSocket JSON protocol in
`coderex-server`** (`crates/server`) instead — see `docs/server-protocol.md`
for the wire format and `docs/server.md` for the daemon itself.

> Note: the crate and its public types were previously named `coderex-sdk` /
> `Sdk*`, which made the in-process bus look like the external SDK surface. The
> crate is now `coderex-events` with `BusEvent` / `EventBus` / `BusSubscriber` /
> `EventSession` types; the persisted directory `~/.coderex/sdk_sessions/` keeps
> its historical name to avoid orphaning existing state.

## Goal

Fan session events out to in-process subscribers, and track the permission and
elicitation state that a session accumulates.

## Design

### Key Types

- **`BusEvent`** (`events.rs:39`) — Non-exhaustive enum of session events with `Serialize`/`Deserialize` and `#[serde(tag = "type", rename_all = "snake_case")]`:
  - `TextDelta` — Streaming text chunk
  - `ToolCallStart` / `ToolCallEnd` — Tool call lifecycle
  - `CompactBoundary` — Context compaction marker
  - `PermissionRequest` / `PermissionResolved` — Permission flow
  - `ElicitationRequest` / `ElicitationResolved` — User prompt flow
  - `SessionEnd` — Session termination
  - `Engine` — Wrapped `coderex_core::types::EngineEvent`
- **`EventBus`** (`events.rs:96`) — In-process broadcast channel wrapper (capacity 4096) with emit/subscribe/subscriber_count. Returns `BusSubscriber` for lag tracking.
- **`BusSubscriber`** (`events.rs:150`) — Wraps `broadcast::Receiver` and counts dropped events via `try_recv()`. Exposes `lag()` for backpressure monitoring.
- **`PermissionTracker`** (`events.rs:305`) — Tracks pending and resolved permission requests with auto-incrementing IDs, `created_at` timestamps, `timeout_secs`, and schema validation.
- **`ElicitationTracker`** (`events.rs:449`) — Tracks pending and resolved user elicitations with auto-incrementing IDs, `created_at` timestamps, and `timeout_secs`.
- **`EventSession`** (`events.rs:570`) — Combines event bus, permission tracker, and elicitation tracker behind `Arc`s. Supports persistence, timeout checking, and background timeout tasks.
- **`PermissionSchema`** (`events.rs:231`) — Describes expected argument types for permission validation.
- **`SessionStore`** (`store.rs:19`) — JSON persistence for permission and elicitation state.

### Architecture Patterns

- **Broadcast pattern** for in-process event distribution (`tokio::sync::broadcast` with 4096 capacity).
- **Interior mutability** via `Arc<Mutex<...>>` for session-level trackers.
- **Sequential ID generation** with `usize` counters (thread-safe via Mutex).
- **JSON persistence** via `SessionStore` — saves tracker snapshots to `~/.coderex/sdk_sessions/{session_id}.json`.
- **Timeout auto-resolution** — `EventSession::check_timeouts()` scans expired items every 5s (or on demand).
- **Backpressure tracking** — `BusSubscriber::lag()` counts dropped events; warnings logged on lag.
- **Schema validation** — `PermissionSchema::validate()` enforces type constraints before permission creation.

### Public API Surface

- `BusEvent`, `EventBus`, `EventSession`, `BusSubscriber`
- `PermissionStatus`, `TrackedPermission`, `PermissionTracker`, `PermissionSchema`, `ArgType`, `PermissionValidationError`
- `ElicitationResponse`, `ElicitationTracker`, `TrackedElicitation`
- `SessionStore`, `StoreError`

## Current Status

### What Works

- Event broadcast with multiple in-process subscribers (tested).
- Permission tracker full lifecycle: create (with optional schema validation), resolve, get, list pending/resolved, timeout expiry.
- Elicitation tracker full lifecycle: create (with timeout), resolve, get message/response, timeout expiry.
- `EventSession` as a unified handle with `check_timeouts()`, `spawn_timeout_checker()`, `persist()`, and `load_or_new()`.
- **Serialization** — `BusEvent`, `PermissionTracker`, `ElicitationTracker`, and all sub-types derive `Serialize`/`Deserialize` with tagged JSON.
- **Session persistence** — `SessionStore` saves/loads tracker state to JSON; `EventSession::load_or_new()` hydrates on startup.
- **Timeout handling** — `created_at` + `timeout_secs` on permissions and elicitations; `check_timeouts()` auto-resolves expired items.
- **Backpressure handling** — `BusSubscriber` tracks lag via `try_recv()`; capacity 4096; warnings logged.
- **Schema validation** — `PermissionSchema` with `ArgType` (String, Number, Boolean, Array, Object); validated in `PermissionTracker::create()`.
- **Engine event bridge** — `ReplLoop` creates an `EventSession` and forwards every `EngineEvent` from `QueryEngine` into it as `BusEvent::Engine(...)`. TUI and headless modes both forward events.

### What Is Stubbed/Partial

None — all identified stubs are bridged.

### Tests

- 17 tests in `events.rs` (event stream, permission tracker, elicitation tracker, session, schema validation, timeout expiry, lag tracking).
- 2 tests in `store.rs` (save/load roundtrip, remove).
- 2 doctests (module-level examples compile).

## Gaps for Production

- **No out-of-process consumer.** `EventBus::emit` returns early when
  `receiver_count() == 0`, and today nothing outside the crate calls
  `subscribe_events()`. The CLI emits into the bus but never subscribes, so
  the bus is effectively a no-op fan-out. Any external consumer should be built
  on the `coderex-server` WebSocket protocol, not by reaching into this bus.
