# events Module

## What this crate is (and is not)

`coderex-events` (`crates/events`) is two things, and neither is an in-process
event bus:

1. **`protocol.rs` — a line-delimited wire protocol.** Exactly one JSON object
   per line on an output stream (`stdout`, in the CLI's case). This is the
   `--output-format stream-json` protocol, and the same frame set is what a
   desktop/IDE host would drive the CLI with over the bridge. The producer is
   the agent loop; **every consumer is out of process** — an SDK client reading
   the stream, a desktop/IDE host, or the transcript mirror.
2. **`events.rs` + `store.rs` — session state.** Permission-request tracking,
   user elicitation, timeout auto-resolution, and JSON persistence of that
   state. This bookkeeping is independent of how a session reports its events.

It never opens a socket. The WebSocket JSON protocol an out-of-process client
speaks to the daemon is `coderex-server` (`crates/server`) — see
`docs/server-protocol.md` for the wire format and `docs/server.md` for the
daemon itself.

> Note: the crate and its public types were previously named `coderex-sdk` /
> `Sdk*`. The persisted directory `~/.coderex/sdk_sessions/` keeps its
> historical name to avoid orphaning existing state.

## There is no in-process bus — deliberately

This crate used to define one: `EventBus` / `BusSubscriber` / `BusEvent`, a
`tokio::sync::broadcast` fan-out modelled on the reference's
`System`/`Assistant`/`User`/`Result` message hierarchy. It has been **deleted**,
and the reason matters:

- **Nothing subscribed to it.** `EventBus::emit` returned early on
  `receiver_count() == 0`, and no caller ever subscribed, so every emit was a
  no-op. The bus was dead.
- **The reference has no in-process bus.** `new EventEmitter` appears in its
  bundles only as a third-party dependency. Its `System`/`Assistant`/`User`/
  `Result` taxonomy is a *stream*: one JSON object per line on stdout, consumed
  by processes that are not it.
- **"Wire up a subscriber" was therefore the wrong repair.** The reference's
  producer is an async generator plus a single `onQueryEvent` callback — "one
  producer → one serialized output", not fan-out. Wiring a subscriber into the
  CodeRex bus would have invented an architecture the reference does not have.
  So the crate now models what the reference actually does: a **sink**.

The reference *does* have a real in-process listener registry — the hook chain
over engine events (`tool.call`, `ui.press`, `session.start`). That is
middleware over the engine, a different abstraction from this message stream,
and it is out of scope here.

## The protocol

`ProtocolMessage` (`protocol.rs`) is an internally-tagged enum. The `type`
discriminant is spelled exactly as the reference spells it:

| Frame | `type` | Payload | Notes |
|---|---|---|---|
| `Assistant(AssistantFrame)` | `assistant` | `message` (role + content blocks), `session_id`, optional `parent_tool_use_id` | One model step |
| `User(UserFrame)` | `user` | same shape | Queued user input, and the `tool_result` blocks fed back to the model |
| `System(SystemFrame)` | `system` | **`subtype`**, `session_id`, flattened subtype payload | e.g. `init`, `compact_boundary` |
| `Result(ResultFrame)` | `result` | **`subtype`**, `is_error`, `duration_ms`, `num_turns`, `session_id`, optional `result` | **The turn-complete signal** |
| `StreamEvent(StreamEventFrame)` | `stream_event` | raw API streaming event, `session_id` | Forwarded unmodified |
| `ControlRequest(ControlRequestFrame)` | `control_request` | `request_id`, `request` (`subtype: can_use_tool`, `tool_name`, `input`, `reason`, `allow_session`) | Headless approval request; not a turn frame |
| `ControlResponse(ControlResponseFrame)` | `control_response` | `response` (`subtype: success/error`, `request_id`, `behavior: allow/deny`, optional `message`) | **Input** direction: read from the child's stdin |

### Headless tool approval

With `--print --output-format stream-json` and a positional prompt (so stdin is
free), the headless engine installs a `ConfirmationBus`. A permission `Ask`
emits one `control_request` frame instead of becoming "no confirmation channel
available". The host writes a `control_response` line to the child's stdin
(`allow`, `allow_session`, or `deny`); the turn then continues or records the
denial. Unknown stdin lines are ignored; EOF ends the reader; an unanswered
request leaves the engine waiting, so a host must answer or terminate the
child. `crates/cli/src/bridge_launcher.rs` uses exactly this channel: it
forwards the request over the bridge transport as `permission_request` and
writes the gateway's `permission_response` back as `control_response`. A
control frame never counts as the turn's `result`.

### One frame, one line

`ProtocolMessage::to_line()` returns the frame as a single line, terminated by
exactly one `\n`. A payload that contains a newline is **JSON-escaped by the
serializer**, never emitted raw — otherwise a `text` block containing `\n`
would split one frame across two lines and desynchronize every reader. The
`to_line_is_exactly_one_line_even_when_text_has_newlines` test pins this.

### `result` is the turn-complete signal

A turn emits **exactly one** `result` frame, including when the turn failed
(the reference's own schema says to treat `result` as the end of the turn).
This is enforced by construction rather than by convention:

- `MessageSink::begin_turn(session_id)` returns a `Turn` guard.
- `Turn::emit_turn_complete(self, ResultFrame)` consumes the guard, so it
  cannot run twice.
- `Turn::emit(&ProtocolMessage::Result(..))` is **refused** — the only
  sanctioned path is `emit_turn_complete`.
- Dropping a `Turn` without completing it logs a warning; the reference always
  emits a result, even for a failed turn.

The CLI closes the turn explicitly when the engine task disappears without
handing the engine back: it emits one `error_during_execution` result and keeps
the sink for the next turn. The only zero-result paths left are a failed write
and a caller that drops the guard without completing it.

### The sink

`MessageSink<W: Write>` wraps **one** writer, with no subscription registry.
`emit` writes one line and **flushes per frame** — a host reading the stream
must see a frame when the producer emits it, not when a buffer fills. The CLI
would hand it `stdout` (`StdoutSink` is the alias for that case).

## Known gaps (honest, not copied)

- **`system` subtypes: 64 in the reference, modelled as a string here.** The
  reference has 64 `system` subtypes with entirely different payloads. Copying
  the list would be a promise this crate cannot keep in sync with the
  reference, so `SystemFrame` models only what every system frame shares — the
  `subtype` string and `session_id` — and preserves the rest verbatim in
  `SystemFrame::data`. Typed accessors for specific subtypes can be added when
  a consumer needs them.
- **The `attachment` payload is an unmodelled union.** Content blocks (`text`,
  `tool_use`, `thinking`, `image`, `document`, server-tool blocks, …) are a
  large union in the reference. This crate carries blocks as raw
  `serde_json::Value` and provides `text_block`, `tool_use_block` and
  `tool_result_block` constructors for the three CodeRex emits. Anything else
  round-trips untouched rather than being rejected.
- **`result` subtype is a `String`, not a closed enum.** `result_subtype`
  names `success`, `error_max_turns` and `error_during_execution`; other
  termination reasons pass through.
- **The transcript mirror is not implemented here.** The protocol is the
  reporting path; nothing in this crate writes a transcript.
- **The CLI is the consumer.** `--output-format stream-json` constructs a
  `MessageSink` in `orchestrator.rs` and the headless turn loop in `repl.rs`
  writes frames to it as engine events arrive; see *How the CLI emits the
  stream* below.

## Design

### Public API surface

- `protocol`: `ProtocolMessage`, `AssistantFrame`, `UserFrame`, `SystemFrame`,
  `ResultFrame`, `StreamEventFrame`, `MessageBody`, `MessageSink`,
  `StdoutSink`, `Turn`, `TurnError`, `result_subtype`, and the
  `text_block` / `tool_use_block` / `tool_result_block` constructors.
- `events`: `PermissionTracker`, `PermissionStatus`, `TrackedPermission`,
  `PermissionSchema`, `ArgType`, `PermissionValidationError`,
  `ElicitationTracker`, `TrackedElicitation`, `ElicitationResponse`,
  `SessionState`, `ResolvedTimeouts`.
- `store`: `SessionStore`, `StoreError`.

### Patterns

- **One producer → one serialized output.** No fan-out, no registry.
- **Ownership as enforcement.** The `Turn` guard makes "exactly one result"
  structural: `Turn::emit` refuses a `result`, `emit_turn_complete` consumes
  the guard, `MessageSink::emit` refuses a `result` from outside a turn, and
  dropping a turn without one writes an `error_during_execution` result, so a
  host is never left waiting for zero.
- **Untyped passthrough at the edges.** Fields the reference does not fix are
  preserved as JSON rather than dropped or re-modelled.
- **Interior mutability** via `Arc<Mutex<..>>` for session trackers; **JSON
  persistence** via `SessionStore` to
  `~/.coderex/sdk_sessions/{session_id}.json`.
- **Timeout auto-resolution.** `SessionState::check_timeouts()` resolves
  expired requests and returns their ids; it does not emit anything itself.

### What was deleted vs kept

- **Deleted:** `BusEvent`, `EventBus`, `BusSubscriber`, `EventSession` (the bus
  field plus `emit` / `subscribe_events`), the `tokio::sync::broadcast` fan-out,
  and the `coderex-core` dependency (its only use was the `BusEvent::Engine`
  variant).
- **Kept, and why:** the permission and elicitation trackers, `PermissionSchema`,
  and `store.rs` are genuine session bookkeeping with no dependency on the bus —
  they had working logic and tests. `EventSession` was renamed
  `SessionState`: it still bundles the two trackers with timeout checking and
  persistence, but it no longer carries events, and the old name asserted a bus
  that no longer exists. `check_timeouts()` now *returns* the ids it resolved
  instead of emitting bus events.

## How the CLI emits the stream (`--output-format stream-json`)

`--output-format` accepts `text` (default), `json`, `markdown`, and
`stream-json`. For `stream-json`, `run_headless_with_prompt` in
`crates/cli/src/orchestrator.rs` constructs
`MessageSink::new(std::io::stdout())` and hands it to the `ReplLoop`
(`with_protocol_sink`) before the turn starts. The headless turn loop in
`repl.rs` then:

- maps engine events to protocol frames as they arrive (`protocol_frame`), so
  the host sees the turn live rather than in one blob at the end;
- opens the turn with `sink.begin_turn(session_id)`;
- closes it with `emit_turn_complete`: `success` for a completed turn,
  `error_max_turns` when the engine stopped at the turn limit, and
  `error_during_execution` (with the reason in `data.error`) for a failed
  query.

`format_headless_output` is skipped for `stream-json`; the final `result`
frame is the answer, one JSON object per line. The CLI test
`stream_json_writes_one_frame_per_line` drives the real `coderex` binary
against the static mock provider and parses every line as JSON, asserting
exactly one `result` frame.
