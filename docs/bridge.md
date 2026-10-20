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
| `heartbeat.rs` | Heartbeat, HeartbeatLoop, WsReconnector, SequenceCursor | Connection health monitoring, exponential backoff reconnect, downstream resume point |
| `epoch.rs` | WriteFence, StaleEpoch | Monotonic worker-epoch write fence |
| `relay.rs` | RelayApprovalGate, RelayCallBody, RelayDecision, `forwardable_headers` | Relayed-call approval binding, header forwarding policy, gateway refusal shape |
| `gateway.rs` | PermissionFrameRelay | Permission-frame relay the bridge drives on its transport: forwards `permission_request`/`permission_response`, tracks outstanding requests, refuses unknown/duplicate answers |
| `state.rs` | BridgeSessionStateMachine | 11-state session lifecycle with validated transitions |
| `permission.rs` | BridgePermissionRouter | Session-scoped Allow/Deny permission routing |
| `poller.rs` | BridgeWorkPoller, SpawnMode | Control plane registration, capacity-aware assignment queue |
| `spawner.rs` | BridgeSessionSpawner | Session creation: worktree, single-dir, same-dir, SSH remote |
| `archive.rs` | BridgeArchiveWriter, BridgeCleanupManager | Versioned transcript archival, resource teardown |
| `delta_sync.rs` | StateDelta, LamportTimestamp, ConcurrentStateTracker | Hash-based state diffing, Lamport conflict resolution |
| `orchestrator.rs` | Bridge, BridgeConfig, BridgeEvent | Central orchestrator owning all modules, event bus, relay endpoint, transport serve loop |

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

**`BridgeSessionStateMachine`** (`state.rs`) — 10 states with validated transitions:

```
Assigned → Preparing → CreatingWorktree → Spawning → Running → AwaitingPermission → Archiving → CleaningUp → Completed/Failed
```

Terminal states: `Completed`, `Failed`. All transitions validated with error types for invalid moves. History tracking for audit.

**`BridgePermissionRouter`** (`permission.rs`) — In-memory router keyed by session ID. Lifecycle: `create_request(session, tool, args)` → `respond(request_id, Allow/Deny)` → `prune_resolved()`. Session-scoped queries.

### Work Polling

**`BridgeWorkPoller`** (`poller.rs`) — Manages the control plane relationship:

- `register()` — `POST /api/environments/register` with env_id, max_concurrent, capabilities.
- `fetch_assignments()` — `GET /api/environments/{id}/assignments` — pulls pending work and enqueues.
- `claim_assignment()` — `POST /api/environments/{id}/assignments/{assignment_id}/claim` — compare-and-set before a session runs. Returns `ClaimOutcome::Claimed` or `ClaimOutcome::Lost`; 404/409/422 are treated as lost (skip and free the slot), while any other failure fails the session and frees the slot.
- `report_capacity_to_server()` — `POST /api/environments/{id}/capacity` — reports current load.
- `take_assignment()` — Dequeues work only when `active_sessions < max_concurrent`.

The claim route is a template (`{env_id}`, `{assignment_id}`), defaulting to the
route above and overridable with `--claim-path` / `CODEREX_BRIDGE_CLAIM_PATH`.

**`SpawnMode`** — Enum: `SingleSession`, `SameDir`, `Worktree`, `Ssh { host, remote_path, identity_file }`. With `as_str()` / `from_str()` for serialization.

### Session Spawning

**`BridgeSessionSpawner`** (`spawner.rs`) — Creates execution contexts per mode:

- **`SingleSession`** / **`SameDir`** — Uses existing directory, prepares session data dir.
- **`Worktree`** — `git worktree add` at `.coderex/worktrees/{session_id}`. Cleanup via `git worktree remove`.
- **`Ssh`** — Connects to remote host via `openssh` crate, creates session directory, verifies `coderex` is available. Verifies host keys with `KnownHosts::Add`: unknown hosts are recorded on first contact and must match afterwards. The `openssh` crate is Unix-only, so the dependency is `cfg(unix)` and Windows builds refuse `SpawnMode::Ssh` with a clear error instead of failing to compile.

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
| `claim_assignment()` | Claim a taken assignment with the control plane |
| `release_assignment_slot()` | Free the slot for an assignment that was lost before spawning |
| `spawn_session()` | Spawn session via spawner, track in state machine |
| `complete_session()` | Drive state to Archiving → CleaningUp, archive transcript and usage, cleanup |
| `fail_session()` | Mark state Failed, free capacity |
| `request_permission()` / `respond_to_permission()` | Permission routing with event emission |
| `handle_relay_call()` | The relay endpoint: forwarding policy, then the approval gate |
| `handle_transport_message()` | Route one inbound frame by type |
| `serve_transport()` | The live receive loop; sequences, routes, answers |
| `stop_handle()` / `request_stop()` | Gracefully stop the serve loop from another task |
| `reconnect_with_resume()` | Reconnect after `stream_end` and ask for the gap |
| `apply_session_write()` | Epoch-fenced write-back from a worker |
| `resume_request()` | The `resume` frame for the last delivered sequence |
| `subscribe()` | Get `broadcast::Receiver<BridgeEvent>` |
| `heartbeat_status()` / `active_sessions()` / `all_recovery_states()` | Observability |
| `connection_id()` / `is_running()` / `pending_assignment_count()` / `pending_permission_count()` | Status queries |

**`BridgeConfig`** — `{ env_id, max_concurrent, control_plane_url, repo_root, worktree_base, session_base, archive_base, heartbeat }`.

**`BridgeEvent`** — 9 broadcastable variants: `SessionSpawned`, `SessionCompleted`, `SessionFailed`, `PermissionRequested`, `PermissionResolved`, `TransportReconnecting`, `TransportReconnected`, `HeartbeatChanged`, `SessionArchived`. Event bus uses `tokio::sync::broadcast` with 256-slot channel.

## The relay endpoint

A cloud session does not reach this machine directly; its requests arrive
through a gateway. Holding a permission grant is not enough at that boundary —
the gateway has to know a call is *the one the person just confirmed*, not a
call injected into a session that already holds a grant. The bridge is where
those requests land, so the bridge is where the approval chain is enforced.

A relayed call is a `relay_call` message on the transport protocol (the same
`TransportMessage` JSON the WebSocket transport speaks):

```json
{
  "type": "relay_call",
  "correlation_id": "call_1",
  "payload": "{\"session_id\":\"sess_1\",\"tool\":\"Bash\",\"args\":\"{\\\"command\\\":\\\"ls\\\"}\"}",
  "metadata": {
    "x-ccr-relay-tool-use-id": "perm_0",
    "x-ccr-relay-approved-intent": "<base64url JSON snapshot>"
  }
}
```

`Bridge::handle_transport_message` routes it to `Bridge::handle_relay_call`,
which runs the two halves of the contract in order:

1. **Forwarding policy first.** `forwardable_headers` keeps only the header
   families the relay defines (`x-frame-*`, `x-ccr-relay-*`) plus `accept` and
   `if-match`, lowercasing what survives. Everything else — `authorization`,
   `cookie`, `x-api-key`, anything the caller invented — is dropped before the
   gate sees it. The allowlist is policy, not data: a pair header smuggled
   under a name the relay does not own arrives as *no header at all*.
2. **The gate.** The surviving headers go through
   `RelayApprovalGate::authorize_headers`, which requires both
   `x-ccr-relay-tool-use-id` and `x-ccr-relay-approved-intent`, decodes the
   snapshot, and matches it against a confirmation the person gave. A
   confirmation authorises **one** call: it is spent on success, so a replay of
   the same pair is refused.

The answer is a `relay_result` frame carrying a `RelayDecision`:

```json
{"outcome": "forward", "session_id": "sess_1", "tool": "Bash", "args": "...", "headers": {...}}
{"outcome": "refuse",  "status": 403, "code": "write_relay_unapproved", "message": "..."}
```

A refusal is the gateway's own shape — `403` with error code
`write_relay_unapproved` and a message telling the caller to retry so the
confirmation card is shown again.

Approvals come from `Bridge::respond_to_permission`: when the person allows a
permission request, the request becomes the confirmation a relayed call for
that call has to present. `withdraw_relay_approval` drops one whose request was
cancelled.

## Worker epoch write fence

More than one worker can serve the same session across a restart, a reconnect,
or a hand-over: the gateway starts a replacement while the old worker is still
finishing its last request. `epoch::WriteFence` remembers the newest
`worker_epoch` it has admitted for a session and refuses anything older.

A worker reports state with a `session_write` frame:

```json
{"type": "session_write", "payload": "{\"session_id\":\"s1\",\"state\":\"running\"}", "metadata": {"worker_epoch": "2"}}
```

`Bridge::apply_session_write` admits the write only if `epoch >=` the fence's
current epoch; a stale worker gets `{"accepted": false, "reason": "...", "current_epoch": 2}`
and the session's state is left with the worker that actually owns it.

The epoch comes from the registration response (`worker_epoch`, also accepted as
`workerEpoch`); the launcher stores it and queues a `session_write` with state
`running` after a session spawns and `completed`/`failed` when it finishes. If
the control plane did not return an epoch, the launcher logs it once and keeps
running without producing writes — a write without an epoch is refused
everywhere and would be noise rather than fencing.

## Stream resume

Reconnecting with exponential backoff re-establishes the connection but not the
stream: whatever the gateway sent while the worker was away is gone.
`heartbeat::SequenceCursor` is the other half of the insurance. Every inbound
frame's `sequence_num` metadata advances the cursor, and a reconnect sends a
`resume` frame asking for the gap rather than the whole stream:

```json
{"type": "resume", "payload": "{\"from_sequence_num\": 4}", "metadata": {"last-event-id": "3"}}
```

Both spellings of the same point: `from_sequence_num` for the bridge's own
protocol, and `Last-Event-ID` for an event stream that only knows that one.

`SessionRecoveryState` now carries `last_sequence_num`, so a recovery snapshot
says not only where a session was but where its stream had got to.

## Transport serve loop

`Bridge::serve_transport` is the loop the relay endpoint lives on. It receives
frames, sequences them, routes each by type, and answers:

| Inbound `type` | Answer | What happens |
|----------------|--------|--------------|
| `relay_call` | `relay_result` | Forwarding policy, then the approval gate |
| `session_write` | `session_write_result` | Epoch-fenced write-back |
| `heartbeat` | `heartbeat_ack` | Records a successful beat |
| `stream_end` | — | Reconnect via `reconnect_with_resume` and send `resume` |
| anything else | `error` | Unknown message type |

## Running the bridge

The production entry point is the `bridge` subcommand:

```sh
coderex bridge \
  --control-plane-url https://control.example.com \
  --env-id builder-01
```

It resolves the configuration, builds the transport through
`coderex_bridge::transport::default_factory()`, constructs a `Bridge`, connects
the transport, and registers the environment with the control plane
(`POST /api/environments/register`) using bounded exponential backoff. It then
runs two loops side by side until Ctrl+C (or SIGTERM on Unix):

- **Transport loop** — `Bridge::serve_transport()` answers heartbeats and
  handles relay, write, and permission frames.
- **Assignment loop** — every `--poll-interval-secs`, fetch assignments, take
  as many as the remaining `--max-concurrent` slots allow, claim each one with
  the control plane, spawn the claimed ones, and report capacity. A lost claim
  (already claimed or gone) is skipped and its slot released without recording
  a session failure; a claim error fails the session and frees the slot. Each
  spawned assignment runs in its own Tokio task through an injectable
  `SessionRunner`, so sessions execute concurrently while the transport and
  poll loops stay responsive. A completion or failure is delivered back to the
  worker, which archives the session (`complete_session`) or records the
  failure (`fail_session`); both free the capacity slot.

There is no fallback to a no-op or in-process transport: an unknown transport
kind and a failed connection both exit non-zero with the reason. Registration
failure is reported after five attempts, and a shutdown during the retry wait
ends the worker cleanly.

### Flags and environment

| Flag | Environment | Default | Meaning |
|------|-------------|---------|---------|
| `--control-plane-url URL` | `CODEREX_BRIDGE_CONTROL_PLANE_URL` | required | Control-plane HTTP API base URL |
| `--env-id ID` | `CODEREX_BRIDGE_ENV_ID` | required | Environment identifier registered with the control plane |
| `--repo-root DIR` | `CODEREX_BRIDGE_REPO_ROOT` | current directory | Git repository the spawner works from |
| `--worktree-base DIR` | `CODEREX_BRIDGE_WORKTREE_BASE` | `<repo>/.coderex/worktrees` | Where session worktrees are created |
| `--session-base DIR` | `CODEREX_BRIDGE_SESSION_BASE` | `<repo>/.coderex/sessions` | Where session data is written |
| `--archive-base DIR` | `CODEREX_BRIDGE_ARCHIVE_BASE` | `<repo>/.coderex/archives` | Where completed sessions are archived |
| `--max-concurrent N` | `CODEREX_BRIDGE_MAX_CONCURRENT` | `5` | Maximum concurrent sessions |
| `--transport KIND` | `CODEREX_BRIDGE_TRANSPORT` | `websocket` | `websocket`/`ws`, or `in-process` |
| `--transport-url URL` | `CODEREX_BRIDGE_URL` | control-plane URL with `http`→`ws` / `https`→`wss` | Gateway WebSocket endpoint |
| `--connection-id ID` | `CODEREX_BRIDGE_CONNECTION_ID` | environment ID | Connection label reported by the transport |
| `--auth-token TOKEN` | `CODEREX_BRIDGE_TOKEN` | none | Bearer token for the transport `Authorization` header |
| `--heartbeat-interval-secs N` | `CODEREX_BRIDGE_HEARTBEAT_INTERVAL_SECS` | `30` | Heartbeat interval |
| `--heartbeat-timeout-secs N` | `CODEREX_BRIDGE_HEARTBEAT_TIMEOUT_SECS` | `10` | Per-heartbeat timeout |
| `--heartbeat-degraded-threshold N` | `CODEREX_BRIDGE_HEARTBEAT_DEGRADED_THRESHOLD` | `2` | Missed beats before `Degraded` |
| `--heartbeat-lost-threshold N` | `CODEREX_BRIDGE_HEARTBEAT_LOST_THRESHOLD` | `5` | Missed beats before `Lost` |
| `--poll-interval-secs N` | `CODEREX_BRIDGE_POLL_INTERVAL_SECS` | `5` | Seconds between assignment fetches and capacity reports |
| `--claim-path PATH` | `CODEREX_BRIDGE_CLAIM_PATH` | `/api/environments/{env_id}/assignments/{assignment_id}/claim` | Claim route template; must contain `{assignment_id}` |

The historical `CODEREX_BRIDGE_URL=ws://host:port` spelling is the transport
URL, not the control-plane API URL. If the gateway does not share the control
plane's host and path, set `--transport-url` (or `CODEREX_BRIDGE_URL`)
explicitly.

### Authentication

The WebSocket transport sends `Authorization: Bearer <token>` when
`--auth-token`/`CODEREX_BRIDGE_TOKEN` is set. The same token is sent by
`HttpControlPlaneClient` on registration, assignment, and capacity requests.
The control plane may also be left unauthenticated when no token is configured;
in that case it is expected to be reachable from the worker's network position.
Tokens are never logged; pass them through the environment where possible.

### Shutdown

`Bridge::stop_handle()` returns a cloneable `BridgeStopHandle`; `stop()` sets
the flag and wakes the serve loop out of its blocking receive, so a signal
handler or supervisor can stop a bridge while `serve_transport` holds
`&mut Bridge`. The subcommand installs a Ctrl+C/SIGTERM listener and calls the
handle; the same stop request cancels registration retries, ends assignment
polling, wakes `serve_transport`, and then the worker disconnects the
transport. Messages already being handled are completed before the loop exits.

Shutdown also stops every running session before releasing its slot. Each
session task owns a `SessionStop`; the worker cancels all of them, then waits
up to `SESSION_TASK_SHUTDOWN_GRACE` (6 s) for the tasks to finish. The default
runner reacts by sending `SIGTERM` to the child on Unix (via the POSIX `kill`
utility, because the workspace denies `unsafe`), or terminating it on Windows,
waiting up to `SESSION_SHUTDOWN_GRACE` (5 s), and then killing it. A runner
that ignores the signal is aborted when the worker's grace expires. Children
are also spawned with `kill_on_drop`, so an aborted task cannot leave an
orphan agent turn behind.

### Session execution

Spawned assignments run through `SessionRunner`. The default
`ProcessSessionRunner` invokes the current executable as

```sh
coderex --print --output-format stream-json -- <prompt>
```

with the prompt as one positional argument so no shell interprets it, and with
the session's work directory as the child's current directory. Stdin is piped
too: it is the return path for tool approvals. Stdout and stderr are piped and
drained by separate tasks, so neither pipe can block the other or the child.
Each stdout/stderr line is forwarded to the transport as a `session_output`
frame, to tracing (debug/warn), and to the local transcript/`stderr.log`.

**Tool approval.** The child emits `control_request`/`can_use_tool` frames
(the headless protocol described in `docs/events.md`). The runner passes each
one to the worker loop, which registers it with the bridge as
`permission_request` (preserving the child's request id) and flushes it over
the transport immediately. When the gateway answers with `permission_response`,
`BridgeEvent::PermissionResolved` tells the launcher to write a
`control_response` line to the child's stdin; the turn then continues or
records the denial. An unanswered request is denied after `CODEREX_BRIDGE_APPROVAL_TIMEOUT_SECS`
(default 300; `0` disables the timeout) with a message saying the approval
timed out — not that the user denied it. Shutdown drops the responders.

**The relay on the production path.** Both frame kinds are routed through
`PermissionFrameRelay` (`crates/bridge/src/gateway.rs`) on the worker's own
control-plane transport. `Bridge::send_pending_permission_requests` relays each
outbound `permission_request` before it goes on the wire — the relay records
the request id as outstanding and refuses an id-less or repeated request — and
`Bridge::handle_transport_message` relays each inbound `permission_response`
before it resolves anything. The frame is forwarded verbatim, so the
correlation id, the payload `request_id` and the session id the child chose are
all preserved. An answer for an id that is not outstanding, or a replay of one
already answered, is refused: it resolves nothing and never reaches the child a
second time. A request the worker stops waiting on — a timeout
(`Bridge::expire_permission`) or an answer the worker resolved itself — is
forgotten by the relay (`PermissionFrameRelay::forget`), so a late answer for
it is refused too. The relay's pending set lives on the `Bridge`, so it
survives a transport reconnect: a request the worker is still waiting on stays
answerable across it. Frames the relay does not own are returned as `None` so
the caller passes them through unchanged.

**What remains outside this repository.** `PermissionFrameRelay` is the in-repo
half of the gateway; the gateway *service* is not here. It still owns the host
connection around the relay: authenticating it, fanning frames out to the right
human client, presenting the approval UI, and deciding what to do about a
request that is never answered.

The turn's final `result` frame supplies `UsageStats`:
`usage.input_tokens`, `usage.output_tokens`,
`usage.cache_read_input_tokens` (or `cache_read_tokens`),
`usage.cache_creation_input_tokens` (or `cache_creation_tokens`), and
`total_cost_usd` (or `cost_usd`). The complete stdout transcript (one JSON
object per line) is written to `<session-base>/<session-id>/transcript.jsonl`;
`complete_session` archives the same transcript and the real usage. Exit status
zero with no error `result` frame is success; a non-zero status, an `is_error`
result, a spawn failure, or a pipe error is reported with the exit code and a
short stderr excerpt.

The child inherits the worker's environment, so provider credentials must be
available there; no credentials from the worker's control-plane token are
added. A deployment that runs sessions differently (a container, an SSH
command, or live forwarding over the transport) injects its own
`SessionRunner` through `run_with_control_plane`; neither the bridge crate nor
the CLI parser needs to change.

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
- **Heartbeat with reconnect** — `HeartbeatLoop` runs async ping; `WsReconnector` provides exponential backoff (1s→30s, 10 max attempts), integrated into `serve_transport` / `reconnect_with_resume` (the old `handle_transport_disconnect` was removed).
- **State machine lifecycle** — 10 states with validated transitions, terminal guards, history audit trail. Production drives `Assigned → Preparing → [CreatingWorktree] → Spawning → Running → [AwaitingPermission ⇄ Running] → Archiving → CleaningUp → Completed`, and any non-terminal state → `Failed` on a runner error. `CreatingWorktree` is entered only for worktree spawns.
- **Multi-mode spawning** — Local worktree isolation, single-dir, same-dir, and SSH remote spawning via `openssh` crate.
- **Capacity-aware polling** — Work assignments only dispensed when `active_sessions < max_concurrent`.
- **Session-scoped permissions** — Permission requests tracked per session with Allow/Deny responses and prune lifecycle.
- **Versioned archival** — `CURRENT_ARCHIVE_VERSION` constant guards deserialization; future versions rejected on load.
- **Delta sync with conflict resolution** — SHA-256 revision hashing + Lamport timestamps for concurrent edit ordering. Last-writer-wins with origin ID tie-breaking.
- **Event bus** — `tokio::sync::broadcast` channel (256 slots) for real-time event streaming to observers.
- **Control plane HTTP API** — Raw HTTP/1.1 server in `coderex-server` serving register, assignments, claim, capacity, and list endpoints.

## Deliberately not built

- **An HTTP relay endpoint.** The reference exposes the relay over HTTP with
  the paired headers as real request headers, and this workspace has no HTTP
  framework (no `hyper`/`axum`/`warp`). Rather than add one, the relay contract
  was hung on the transport protocol the bridge already speaks. The binding
  rules, the wire format, and the refusal are unchanged; only the carrier is
  different.
- **A relay endpoint on the `coderex-server` WebSocket protocol.** The server's
  WebSocket surface is a local SDK/daemon surface with its own API-key auth; it
  is not gateway-mediated, so there is nothing there for a relayed call to
  arrive through. The relay lives in `coderex-bridge`, which is the component a
  gateway forwards to. The two crates are siblings with no dependency edge
  between them, so the endpoint could not be put on the server without either
  adding one or duplicating the gate.
- **Actually executing a resolved command server-side.** The server enforces
  which commands a remote/bridge session may resolve; it does not run them.
  Execution belongs to the session's tool executor, which the daemon does not
  own, and giving the daemon shell execution would be a new attack surface
  rather than a port of an existing behaviour.
- **Server-sent-events framing for the downstream.** The resume point is
  carried as a sequence number plus `Last-Event-ID`; there is no SSE encoder in
  this workspace and the transport is WebSocket, so the two spellings of the
  resume point are what the gateway is given.
- **A separate control-plane credential.** The same `--auth-token` /
  `CODEREX_BRIDGE_TOKEN` is used for the gateway transport and the control-plane
  HTTP API. There is no second credential setting, so a deployment that issues
  different tokens for the two must front the control plane to translate them.
- **Live output over the bridge transport.** The default launcher forwards each
  child stdout/stderr line as a `session_output` frame, records it in tracing,
  and keeps the local transcript/`stderr.log`; the transcript is archived on
  completion. A custom `SessionRunner` can produce output through the same
  channel.
- **A gateway path convention.** The default transport URL rewrites the
  control-plane scheme and keeps the same host and path. A gateway mounted on a
  different path must be given `--transport-url` / `CODEREX_BRIDGE_URL`.

## Test Summary

| Module | Test Count | Coverage |
|--------|-----------|----------|
| `transport` | 15 | InProcessTransport bidirectional messaging, timeouts, connection ID, configured factory |
| `ws_transport` | 8 | WebSocket connect/reconnect, auth, token store CRUD, JSON roundtrip |
| `heartbeat` | 19 | Healthy→Degraded→Lost transitions, backoff, recovery state, loop, sequence cursor |
| `epoch` | 4 | Worker epoch write fence: newer epoch takes it, stale epoch refused |
| `relay` | 13 | Approval binding, one-time consumption, forwarding policy, refusal shape |
| `state` | 17 | All 11 state transitions, terminal guards, failure, history |
| `permission` | 7 | Create/respond/query/prune, Allow/Deny, session scoping |
| `poller` | 15 | Capacity limits, assignment queue, register, URL config, injected control-plane client, claim outcomes and path templates |
| `spawner` | 14 | Worktree create/remove, single session, SSH variant, non-existent cleanup |
| `teleport` | 9 | Snapshot/restore, manifest, conflict handling |
| `archive` | 10 | Versioning, list/load, metadata, process tracking, cleanup report |
| `delta_sync` | 21 | Diff/apply, revision hash, JSON roundtrip, Lamport timestamps, conflict tracker, delta merge |
| `orchestrator` | 36 | Connect/disconnect, spawn/complete lifecycle, permissions, events, recovery, relay endpoint, relayed permission frames, write fence, resume, stop handle |
| `gateway` | 13 | Permission-frame relay: worker→host and host→worker forwarding, unknown/duplicate refusal, forget, in-process transport round trip |
| `bridge_launcher` (cli) | 31 | Flag/env mapping, defaults, required-value errors, injectable transport/control plane/runner, registration, assignment claim success/lost/failure, spawn, session completion/failure and slot release, streaming stdout/stderr, usage and transcript capture, child permission round trip through the relay, capacity reporting, graceful session shutdown and task cancellation |
| `bridge_integration` (tests/) | 33 | Live relay call over WebSocket, refusal, stale epoch, resume cursor, end-to-end flows |
| `control_plane` (server) | 16 | All 5 HTTP endpoints, state management, claim conflicts, 404 routing |
| `filter` (server) | 7 | Remote/bridge shell whitelists, dangerous-command rejection |
| `persistence` (server) | 6 | Session store roundtrip, salted fingerprints, legacy-token migration |
| `session` (server) | 26 | Auth, roles, command resolution, rate limiting, idle cleanup, protocol |
| `server_integration` (server, tests/) | 24 | Full WS lifecycle, auth, roles, command whitelist over the protocol |

**Total: 201 bridge unit + 31 CLI launcher unit + 33 bridge integration + 55 server unit + 24 server integration = 344 tests pass.**

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
