# bridge Module

## Goal

Remote session orchestration for connecting to remote CodeRex instances or managed environments. The bridge manages the full lifecycle of remote coding sessions: connecting via transport, receiving work assignments from a control plane, spawning sessions (locally via worktrees or remotely via SSH), routing tool-call permissions, synchronizing state via delta sync with conflict resolution, and archiving completed sessions with versioned transcripts.

## Architecture

```
┌────────────────────────────── coderex-bridge ──────────────────────────────┐
│                                                                            │
│  ┌──────────────┐  ┌────────────────┐  ┌────────────────────────────┐     │
│  │ Transport    │  │   Heartbeat    │  │    State Machine           │     │
│  │ • trait      │  │   • Healthy /  │  │    11 validated            │     │
│  │ • InProcess  │  │     Degraded / │  │    transitions             │     │
│  │ • WebSocket  │  │     Lost       │  │    + history audit         │     │
│  └──────┬───────┘  └────────────────┘  └────────────────────────────┘     │
│         │                                                                  │
│  ┌──────▼───────────────────────────────────────────────────────────┐     │
│  │                    Bridge (orchestrator)                          │     │
│  │  • owns all modules below                                        │     │
│  │  • connect() / disconnect() via Transport                        │     │
│  │  • spawn_session() / complete_session() / fail_session()         │     │
│  │  • request_permission() / respond_to_permission()                │     │
│  │  • subscribe() → broadcast::Receiver<BridgeEvent>                │     │
│  └──────┬───────┬──────────────┬──────────┬──────────────────┬──────┘     │
│         │       │              │          │                  │             │
│  ┌──────▼──┐ ┌──▼────────┐ ┌──▼────────┐│  ┌───────────┐ ┌─▼──────────┐  │
│  │ Poller  │ │ Spawner   │ │ Permission││  │ DeltaSync │ │ Archive    │  │
│  │ • HTTP  │ │ • worktree│ │  Router   ││  │ • diff    │ │ • versioned│  │
│  │   API   │ │ • SSH     │ │  Allow/   ││  │ • Lamport │ │ • usage    │  │
│  │ • cap-  │ │ • session │ │  Deny     ││  │   ts      │ │   stats    │  │
│  │   aware │ │  dirs     │ │  scoped   ││  │ • merge   │ │ • cleanup  │  │
│  └─────────┘ └───────────┘ └───────────┘│  └───────────┘ └────────────┘  │
│                                          │                                  │
│  ┌──────────────────────────────────────┐  ┌───────────────────────────┐   │
│  │ BridgeTokenStore                      │  │ WsReconnector             │   │
│  │ ~/.coderex/bridge_tokens.json         │  │ exponential backoff       │   │
│  │ CRUD: upsert / remove / validate      │  │ 1s base → 30s max, 10x   │   │
│  └──────────────────────────────────────┘  └───────────────────────────┘   │
└────────────────────────────────────────────────────────────────────────────┘
```

### Control Plane Server (`coderex-server` crate)

```
┌──────────────────── coderex-server ─────────────────────┐
│                                                         │
│  WebSocket Server          Control Plane HTTP API       │
│  • ws://host:port          • POST /api/environments/    │
│  • auth via API key          register                   │
│  • session management      • GET  /api/environments/    │
│  • rate limiting             {id}/assignments           │
│  • idle cleanup            • POST /api/environments/    │
│                              {id}/assignments/{id}/claim │
│  Health Endpoint           • POST /api/environments/    │
│  • GET /health               {id}/capacity              │
│                              • GET  /api/environments   │
└─────────────────────────────────────────────────────────┘
```

## Module Inventory

| File | Module | Responsibility |
|------|--------|---------------|
| `transport.rs` | Transport trait + InProcessTransport | Message transport abstraction, bidirectional channels |
| `ws_transport.rs` | WebSocketTransport + BridgeTokenStore | TLS WebSocket client, token-based auth, credential persistence |
| `heartbeat.rs` | Heartbeat, HeartbeatLoop, WsReconnector | Connection health monitoring, exponential backoff reconnect |
| `state.rs` | BridgeSessionStateMachine | 11-state session lifecycle with validated transitions |
| `permission.rs` | BridgePermissionRouter | Session-scoped Allow/Deny permission routing |
| `poller.rs` | BridgeWorkPoller, SpawnMode | Control plane registration, capacity-aware assignment queue |
| `spawner.rs` | BridgeSessionSpawner | Session creation: worktree, single-dir, same-dir, SSH remote |
| `archive.rs` | BridgeArchiveWriter, BridgeCleanupManager | Versioned transcript archival, resource teardown |
| `delta_sync.rs` | StateDelta, LamportTimestamp, ConcurrentStateTracker | Hash-based state diffing, Lamport conflict resolution |
| `orchestrator.rs` | Bridge, BridgeConfig, BridgeEvent | Central orchestrator owning all modules, event bus |

## Key Types

### Transport Layer

**`Transport` (trait)** — Pluggable message transport with `connect`, `disconnect`, `is_connected`, `send`, `receive`, `receive_timeout`, `connection_id`. Implemented by:

- **`InProcessTransport`** — Bidirectional channel-based transport for testing and local use. `create_pair()` returns two connected transports.
- **`WebSocketTransport`** — WebSocket client via `tokio-tungstenite` with `wss://` TLS (webpki-roots) and `Authorization: Bearer` token auth. Async reader/writer tasks bridge WebSocket stream to internal channels.

**`WsTransportConfig`** — Builder: `new(url)`, `with_auth(token)`.

**`BridgeTokenStore`** — File-based token CRUD for `~/.coderex/bridge_tokens.json`: `upsert`, `remove`, `validate`, `load`/`save`.

### Heartbeat System

- **`HeartbeatStatus`** — `Healthy`, `Degraded`, `Lost`.
- **`HeartbeatConfig`** — `interval_secs`, `degraded_threshold`, `lost_threshold`, `timeout_secs`.
- **`Heartbeat`** — Stateful tracker with `record_success`, `record_failure`, `status`.
- **`HeartbeatLoop`** — Async loop running a user-provided ping function, stoppable via `Notify`.
- **`WsReconnector`** — Exponential backoff: base 1s, max 30s cap, max 10 attempts. `next_delay()`, `reset()`.
- **`SessionRecoveryState`** — Snapshot `{ session_id, state_label }` for reconnect recovery.

### Session Lifecycle

**`BridgeSessionStateMachine`** (`state.rs`) — 11 states with validated transitions:

```
Assigned → Preparing → CreatingWorktree → Spawning → Running → AwaitingPermission → Archiving → CleaningUp → Completed/Failed
```

Terminal states: `Completed`, `Failed`. All transitions validated with error types for invalid moves. History tracking for audit.

**`BridgePermissionRouter`** (`permission.rs`) — In-memory router keyed by session ID. Lifecycle: `create_request(session, tool, args)` → `respond(request_id, Allow/Deny)` → `prune_resolved()`. Session-scoped queries.

### Work Polling

**`BridgeWorkPoller`** (`poller.rs`) — Manages the control plane relationship:

- `register()` — `POST /api/environments/register` with env_id, max_concurrent, capabilities.
- `fetch_assignments()` — `GET /api/environments/{id}/assignments` — pulls pending work and enqueues.
- `report_capacity_to_server()` — `POST /api/environments/{id}/capacity` — reports current load.
- `take_assignment()` — Dequeues work only when `active_sessions < max_concurrent`.

**`SpawnMode`** — Enum: `SingleSession`, `SameDir`, `Worktree`, `Ssh { host, remote_path, identity_file }`. With `as_str()` / `from_str()` for serialization.

### Session Spawning

**`BridgeSessionSpawner`** (`spawner.rs`) — Creates execution contexts per mode:

- **`SingleSession`** / **`SameDir`** — Uses existing directory, prepares session data dir.
- **`Worktree`** — `git worktree add` at `.coderex/worktrees/{session_id}`. Cleanup via `git worktree remove`.
- **`Ssh`** — Connects to remote host via `openssh` crate, creates session directory, verifies `coderex` is available. Uses `openssh::Session::connect()` with `KnownHosts::Accept`.

### Delta Sync & Conflict Resolution

**`StateDelta`** (`delta_sync.rs`) — `{ base_revision, operations: Vec<DeltaOp> }` where `DeltaOp` is `Upsert { key, value }` or `Delete { key }`.

**Hash-based diffing** — `compute_delta(base_rev, old_state, new_state)` returns minimal changes. `compute_revision_hash(state)` produces deterministic SHA-256 hash (sorted keys).

**Lamport timestamps** — `{ counter: u64, origin_id: u32 }` for ordering concurrent operations:

- `tick()` — Increments counter for same origin.
- `merge(a, b)` — Last-writer-wins; tie-break by origin ID (higher wins).
- `happens_after(other)` — Strict ordering check.

**`ConcurrentStateTracker`** — Per-key timestamp tracking. `record_local(key, origin)` produces timestamp for local ops. `record_remote(key, value, remote_ts)` resolves conflicts — returns `ConflictResolution` with winner info.

**`merge_concurrent_deltas(base, delta_a, origin_a, delta_b, origin_b)`** — Merges two concurrent deltas into one, resolving conflicts via Lamport ordering.

### Archive & Cleanup

**`SessionArchive`** — Versioned `{ version, session_id, transcript_path, usage, status, archived_at }`. `CURRENT_ARCHIVE_VERSION = 1`; rejects future versions on load.

**`UsageStats`** — `{ input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, cost_usd }`.

**`BridgeCleanupManager`** — Process tracking by PID, worktree/session directory removal, produces `CleanupReport` with summary.

### Orchestrator

**`Bridge`** (`orchestrator.rs`) — Central orchestrator:

| Method | Purpose |
|--------|---------|
| `new(config, transport)` | Creates bridge, wires all modules |
| `connect()` / `disconnect()` | Transport lifecycle |
| `register()` | Register with control plane |
| `receive_assignment()` / `take_assignment()` | Work queue delegation |
| `spawn_session()` | Spawn session via spawner, track in state machine |
| `complete_session()` | Drive state to Archiving → CleaningUp, archive, cleanup |
| `fail_session()` | Mark state Failed, free capacity |
| `request_permission()` / `respond_to_permission()` | Permission routing with event emission |
| `subscribe()` | Get `broadcast::Receiver<BridgeEvent>` |
| `heartbeat_status()` / `active_sessions()` / `all_recovery_states()` | Observability |
| `connection_id()` / `is_running()` / `pending_assignment_count()` / `pending_permission_count()` | Status queries |

**`BridgeConfig`** — `{ env_id, max_concurrent, control_plane_url, repo_root, worktree_base, session_base, archive_base, heartbeat }`.

**`BridgeEvent`** — 9 broadcastable variants: `SessionSpawned`, `SessionCompleted`, `SessionFailed`, `PermissionRequested`, `PermissionResolved`, `TransportReconnecting`, `TransportReconnected`, `HeartbeatChanged`, `SessionArchived`. Event bus uses `tokio::sync::broadcast` with 256-slot channel.

## Data Flow

```
Control Plane                      Bridge                          Remote/Local
─────────────                      ──────                          ──────────────

  ┌─────────────┐
  │ register()  │ ──────────── POST /api/environments/register ──►
  └─────────────┘                                                    │
                                                                     │
  ┌─────────────┐                                                    │
  │ fetch       │ ◄────────── GET  /api/environments/{id}/assignments│
  │ assignments │                                                    │
  └──────┬──────┘                                                    │
         │                                                           │
         ▼                                                           │
  ┌─────────────┐     spawn()       ┌──────────┐                     │
  │ take        │ ────────────────► │ Spawner  │ ──► worktree / SSH │
  │ assignment  │                   └──────────┘                     │
  └──────┬──────┘                                                    │
         │                                                           │
         ▼                                                           │
  ┌─────────────┐     permissions    ┌───────────┐                   │
  │ request     │ ◄──────────────── │ Permission│ ◄─ tool calls      │
  │ permission  │ ────────────────► │  Router   │                    │
  └──────┬──────┘                   └───────────┘                    │
         │                                                           │
         ▼                                                           │
  ┌─────────────┐     archive       ┌──────────┐                     │
  │ complete    │ ────────────────► │ Archive  │ ──► JSON file       │
  │ session     │                   └──────────┘                     │
  └──────┬──────┘                                                    │
         │                                                           │
         ▼                                                           │
  ┌─────────────┐     cleanup       ┌───────────┐                    │
  │ report      │ ────────────────► │ Cleanup   │ ──► remove dirs    │
  │ capacity    │ ────────────────► │ Manager   │                    │
  └─────────────┘                   └───────────┘                    │
```

## Architecture Patterns

- **Transport abstraction** — Pluggable `Transport` trait with `InProcessTransport` (channels) and `WebSocketTransport` (TLS WebSocket via `tokio-tungstenite`).
- **Heartbeat with reconnect** — `HeartbeatLoop` runs async ping; `WsReconnector` provides exponential backoff (1s→30s, 10 max attempts), integrated into `Bridge.handle_transport_disconnect()`.
- **State machine lifecycle** — 11 states with validated transitions, terminal guards, history audit trail. Sessions progress: `Assigned → Preparing → Spawning → Running → Archiving → CleaningUp → Completed`.
- **Multi-mode spawning** — Local worktree isolation, single-dir, same-dir, and SSH remote spawning via `openssh` crate.
- **Capacity-aware polling** — Work assignments only dispensed when `active_sessions < max_concurrent`.
- **Session-scoped permissions** — Permission requests tracked per session with Allow/Deny responses and prune lifecycle.
- **Versioned archival** — `CURRENT_ARCHIVE_VERSION` constant guards deserialization; future versions rejected on load.
- **Delta sync with conflict resolution** — SHA-256 revision hashing + Lamport timestamps for concurrent edit ordering. Last-writer-wins with origin ID tie-breaking.
- **Event bus** — `tokio::sync::broadcast` channel (256 slots) for real-time event streaming to observers.
- **Control plane HTTP API** — Raw HTTP/1.1 server in `coderex-server` serving register, assignments, claim, capacity, and list endpoints.

## Test Summary

| Module | Test Count | Coverage |
|--------|-----------|----------|
| `transport` | 10 | InProcessTransport bidirectional messaging, timeouts, connection ID |
| `ws_transport` | 7 | WebSocket connect, auth, token store CRUD, JSON roundtrip |
| `heartbeat` | 12 | Healthy→Degraded→Lost transitions, backoff, recovery state, loop |
| `state` | 14 | All 11 state transitions, terminal guards, failure, history |
| `permission` | 6 | Create/respond/query/prune, Allow/Deny, session scoping |
| `poller` | 7 | Capacity limits, assignment queue, register, URL config |
| `spawner` | 5 | Worktree create/remove, single session, SSH variant, non-existent cleanup |
| `archive` | 9 | Versioning, list/load, metadata, process tracking, cleanup report |
| `delta_sync` | 22 | Diff/apply, revision hash, JSON roundtrip, Lamport timestamps, conflict tracker, delta merge |
| `orchestrator` | 13 | Connect/disconnect, spawn/complete lifecycle, permissions, events, recovery |
| `control_plane` (server) | 16 | All 5 HTTP endpoints, state management, claim conflicts, 404 routing |

**Total: 109 bridge tests + 41 server tests = 150 tests pass.**

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime, channels, time |
| `tokio-tungstenite` | WebSocket client with TLS (webpki-roots) |
| `reqwest` | HTTP client for control plane API |
| `openssh` | SSH remote execution via OpenSSH |
| `serde` / `serde_json` | Serialization for all protocol types |
| `sha2` | Revision hash computation (SHA-256) |
| `strum` | Enum derive for state labels |
| `uuid` | Unique session/assignment IDs |
| `chrono` | Timestamp serialization |
| `home` | Home directory resolution for token store |
| `thiserror` | Error type derives |
| `tracing` | Structured logging |
| `async-trait` | Async trait support |
| `tempfile` (dev) | Temporary directories for tests |
