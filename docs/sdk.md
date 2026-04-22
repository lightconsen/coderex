# sdk Module Inspection

## Goal

Provides a structured event-driven API for headless and embedded usage of CodeRex. Bridges the core engine with external consumers by exposing a broadcast-based event stream, permission tracking, user elicitation, session persistence, and schema validation.

## Design

### Key Types

- **`SdkEvent`** (`events.rs:14`) — Non-exhaustive enum of all SDK events with `Serialize`/`Deserialize` and `#[serde(tag = "type", rename_all = "snake_case")]` for clean JSON:
  - `TextDelta` — Streaming text chunk
  - `ToolCallStart` / `ToolCallEnd` — Tool call lifecycle
  - `CompactBoundary` — Context compaction marker
  - `PermissionRequest` / `PermissionResolved` — Permission flow
  - `ElicitationRequest` / `ElicitationResolved` — User prompt flow
  - `SessionEnd` — Session termination
  - `Engine` — Wrapped `coderex_core::types::EngineEvent`
- **`SdkEventStream`** (`events.rs:120`) — Broadcast channel wrapper (capacity 4096) with emit/subscribe/subscriber_count. Returns `SdkSubscriber` for lag tracking.
- **`SdkSubscriber`** (`events.rs:155`) — Wraps `broadcast::Receiver` and counts dropped events via `try_recv()`. Exposes `lag()` for backpressure monitoring.
- **`PermissionTracker`** (`events.rs:264`) — Tracks pending and resolved permission requests with auto-incrementing IDs, `created_at` timestamps, `timeout_secs`, and schema validation.
- **`ElicitationTracker`** (`events.rs:465`) — Tracks pending and resolved user elicitations with auto-incrementing IDs, `created_at` timestamps, and `timeout_secs`.
- **`SdkSession`** (`events.rs:555`) — Combines event stream, permission tracker, and elicitation tracker behind `Arc`s. Supports persistence, timeout checking, and background timeout tasks.
- **`PermissionSchema`** (`events.rs:231`) — Describes expected argument types for permission validation.
- **`SessionStore`** (`store.rs:20`) — JSON persistence for permission and elicitation state.

### Architecture Patterns

- **Broadcast pattern** for event distribution (`tokio::sync::broadcast` with 4096 capacity).
- **Interior mutability** via `Arc<Mutex<...>>` for session-level trackers.
- **Sequential ID generation** with `usize` counters (thread-safe via Mutex).
- **JSON persistence** via `SessionStore` — saves tracker snapshots to `~/.coderex/sdk_sessions/{session_id}.json`.
- **Timeout auto-resolution** — `SdkSession::check_timeouts()` scans expired items every 5s (or on demand).
- **Backpressure tracking** — `SdkSubscriber::lag()` counts dropped events; warnings logged on lag.
- **Schema validation** — `PermissionSchema::validate()` enforces type constraints before permission creation.

### Public API Surface

- `SdkEvent`, `SdkEventStream`, `SdkSession`, `SdkSubscriber`
- `PermissionStatus`, `TrackedPermission`, `PermissionTracker`, `PermissionSchema`, `ArgType`, `PermissionValidationError`
- `ElicitationResponse`, `ElicitationTracker`, `TrackedElicitation`
- `SessionStore`, `StoreError`

## Current Status

### What Works

- Event stream broadcast with multiple subscribers (tested).
- Permission tracker full lifecycle: create (with optional schema validation), resolve, get, list pending/resolved, timeout expiry.
- Elicitation tracker full lifecycle: create (with timeout), resolve, get message/response, timeout expiry.
- `SdkSession` as a unified handle with `check_timeouts()`, `spawn_timeout_checker()`, `persist()`, and `load_or_new()`.
- **Serialization** — `SdkEvent`, `PermissionTracker`, `ElicitationTracker`, and all sub-types derive `Serialize`/`Deserialize` with tagged JSON.
- **Session persistence** — `SessionStore` saves/loads tracker state to JSON; `SdkSession::load_or_new()` hydrates on startup.
- **Timeout handling** — `created_at` + `timeout_secs` on permissions and elicitations; `check_timeouts()` auto-resolves expired items.
- **Backpressure handling** — `SdkSubscriber` tracks lag via `try_recv()`; capacity increased to 4096; warnings logged.
- **Schema validation** — `PermissionSchema` with `ArgType` (String, Number, Boolean, Array, Object); validated in `PermissionTracker::create()`.
- **Comprehensive documentation** — Module-level examples, per-variant rustdoc on `SdkEvent`, inline docs on all public items.
- **Engine event bridge** — `ReplLoop::with_engine()` creates an `SdkSession` and forwards every `EngineEvent` from `QueryEngine` into the SDK session as `SdkEvent::Engine(...)`. TUI and headless modes both forward events.
- **Tool execution bridge** — The `QueryEngine` already emits `ToolCallStart`/`ToolCallEnd` as `EngineEvent`s during tool dispatch. These are bridged into the SDK session, so external subscribers see tool lifecycle events.
- **SDK timeout checker** — `SdkSession::spawn_timeout_checker()` runs a background task that auto-resolves expired permissions and elicitations every 5 seconds.

### What Is Stubbed/Partial

None — all identified stubs are bridged.

### Tests

- 11 tests in `events.rs` (event stream, permission tracker, elicitation tracker, session, schema validation, timeout expiry, lag tracking).
- 2 tests in `store.rs` (save/load roundtrip, remove).
- 2 doctests (module-level examples compile).

## Gaps for Production

None — all identified production gaps are implemented.
