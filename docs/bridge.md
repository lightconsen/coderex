# bridge Module Inspection

## Goal

Remote session orchestration — transport layer, heartbeat, state synchronization, and permission proxy for connecting to remote CodeRex instances or managed environments.

## Design

### Key Types

- **`Transport`** (`transport.rs`) — Message transport abstraction (WebSocket, HTTP).
- **`Heartbeat`** (`heartbeat.rs`) — Keepalive and health checking for remote connections.
- **`Poller`** (`poller.rs`) — Polling-based transport fallback.
- **`Spawner`** (`spawner.rs`) — Remote process/agent spawning.
- **`State`** (`state.rs`) — Shared state synchronization across bridge.
- **`PermissionProxy`** (`permission.rs`) — Forwards permission requests to remote authority.
- **`Archive`** (`archive.rs`) — Session snapshot packaging for transfer.

### Architecture Patterns

- **Transport abstraction** — Pluggable transports (WebSocket, HTTP polling).
- **Heartbeat health checks** — Periodic pings to detect disconnections.
- **State replication** — Bidirectional state sync for remote sessions.

## Current Status

### What Works

- Transport trait and heartbeat structure.
- State synchronization types.
- Archive packaging for session snapshots.
- Permission proxy delegation types.

### What Is Stubbed/Partial

- **Actual WebSocket transport** — Types exist but no full WebSocket client implementation.
- **Remote spawning** — Spawner types defined but no SSH or remote execution.
- **State sync protocol** — No actual wire format for state exchange.
- **Archive extraction** — Can package but not unpack/reconstitute sessions.

## Gaps for Production

- **No encryption in transit**: Transport does not mandate TLS.
  → **Suggestion**: Require `wss://` (WebSocket Secure) for all WebSocket transports. Reject `ws://` connections unless explicitly allowed via `insecure_localhost` config flag. Use `tokio-native-tls` or `tokio-rustls` for certificate validation.
- **No authentication**: Remote connections have no auth mechanism.
  → **Suggestion**: Implement token-based auth: each client provides a pre-shared token in the `Authorization` header during WebSocket handshake. Validate against a token registry in `~/.coderex/bridge_tokens.json`.
- **No reconnection backoff**: Disconnects are fatal; no retry.
  → **Suggestion**: Implement exponential backoff reconnection starting at 1s, capped at 60s. Maintain a `ConnectionState` enum (Connected, Reconnecting, Disconnected). Surface reconnection status in the UI.
- **No bandwidth optimization**: State sync sends full state, not deltas.
  → **Suggestion**: Compute a JSON diff (via `json-patch` or `serde_json_diff`) between local and remote state. Send only the diff. On first connect, send full state; subsequent syncs send patches.
- **No conflict resolution**: Simultaneous edits from local and remote produce undefined behavior.
  → **Suggestion**: Adopt Operational Transformation (OT) or CRDTs for state merging. Assign a Lamport timestamp to each edit. On conflict, prefer the edit with the higher timestamp or apply a last-write-wins policy with user notification.
- **Archive format not versioned**: Format changes break compatibility silently.
  → **Suggestion**: Add a `version: u32` field to the Archive schema. On unpack, check the version. If outdated, attempt migration or emit a clear error. Document the format in `docs/archive-format.md`.
