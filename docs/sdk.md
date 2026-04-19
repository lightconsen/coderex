# sdk Module Inspection

## Goal

Provides a structured event-driven API for headless and embedded usage of CodeRex. Bridges the core engine with external consumers by exposing a broadcast-based event stream, permission tracking, and user elicitation (confirmation prompts) systems.

## Design

### Key Types

- **`SdkEvent`** (`events.rs:14`) — Non-exhaustive enum of all SDK events:
  - `TextDelta` — Streaming text chunk
  - `ToolCallStart` / `ToolCallEnd` — Tool call lifecycle
  - `CompactBoundary` — Context compaction marker
  - `PermissionRequest` / `PermissionResolved` — Permission flow
  - `ElicitationRequest` / `ElicitationResolved` — User prompt flow
  - `SessionEnd` — Session termination
  - `Engine` — Wrapped `coderex_core::types::EngineEvent`
- **`SdkEventStream`** (`events.rs:67`) — Broadcast channel wrapper (capacity 1024) with emit/subscribe/subscriber_count.
- **`PermissionTracker`** (`events.rs:133`) — Tracks pending and resolved permission requests with auto-incrementing IDs (`perm_{N}`).
- **`ElicitationTracker`** (`events.rs:225`) — Tracks pending and resolved user elicitations with auto-incrementing IDs (`elicit_{N}`).
- **`SdkSession`** (`events.rs:294`) — Combines event stream, permission tracker, and elicitation tracker behind `Arc`s.

### Architecture Patterns

- **Broadcast pattern** for event distribution (`tokio::sync::broadcast`).
- **Interior mutability** via `Arc<Mutex<...>>` for session-level trackers.
- **Sequential ID generation** with `usize` counters (thread-safe via Mutex).

### Public API Surface

- `SdkEvent`, `SdkEventStream`, `SdkSession`, `PermissionStatus`, `TrackedPermission`, `PermissionTracker`, `ElicitationResponse`, `ElicitationTracker`.

## Current Status

### What Works

- Event stream broadcast with multiple subscribers (tested).
- Permission tracker full lifecycle: create, resolve, get, list pending/resolved.
- Elicitation tracker full lifecycle: create, resolve, get message/response.
- `SdkSession` as a unified handle.

### What Is Stubbed/Partial

- **No integration with actual tool execution**: `ToolCallStart`/`ToolCallEnd` are emitted but no caller actually emits them in this crate.
- **`Engine` variant**: Wraps `coderex_core::types::EngineEvent` but the core integration is not visible here.

### Tests

- 12 test functions covering event stream, permission tracker, elicitation tracker, and session.
- All pure unit tests with no async I/O.

## Gaps for Production

- **No persistence**: Permission and elicitation states are in-memory only; session restart loses all history.
  → **Suggestion**: Serialize pending permissions and elicitations to SQLite on creation. Load them on session startup. Use `serde_json` to store the full state. Mark resolved entries rather than deleting them for audit trails.
- **No timeout handling**: Pending permissions/elicitations can hang forever.
  → **Suggestion**: Add a `timeout_seconds: u64` field to `TrackedPermission` and `ElicitationRequest`. Spawn a background task that auto-resolves with `PermissionStatus::Denied` or `ElicitationResponse::Timeout` after the deadline. Default: 300s.
- **No backpressure handling**: Broadcast channel drops events if subscribers are slow; no warning or logging.
  → **Suggestion**: Track the `lag_count` (dropped events) per subscriber. If a subscriber's lag exceeds a threshold (e.g., 100 events), log a warning and optionally disconnect the slow subscriber. Consider using `tokio::sync::broadcast` with a larger buffer (e.g., 4096).
- **Missing documentation**: Some variants lack detailed docs.
  → **Suggestion**: Add comprehensive rustdoc comments to every `SdkEvent` variant explaining when it fires, what data it carries, and how consumers should handle it. Include code examples in the module-level documentation.
- **No serialization**: Events are not `Serialize`/`Deserialize`, making cross-process or network usage impossible.
  → **Suggestion**: Derive `Serialize` and `Deserialize` for `SdkEvent` and all sub-types. Use `#[serde(tag = "type")]` for enum variants to produce a clean JSON schema. Gate this behind a `serde` feature flag.
- **Permission args are raw `serde_json::Value`**: No validation or schema enforcement.
  → **Suggestion**: Define a `PermissionSchema` type that describes expected argument types (string, number, boolean, array). Validate the `args` Value against the schema before creating the permission request. Return a validation error if the args don't match.
