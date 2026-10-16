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

- `MessageSink::begin_turn()` returns a `Turn` guard.
- `Turn::emit_turn_complete(self, ResultFrame)` consumes the guard, so it
  cannot run twice.
- `Turn::emit(&ProtocolMessage::Result(..))` is **refused** — the only
  sanctioned path is `emit_turn_complete`.
- Dropping a `Turn` without completing it logs a warning; the reference always
  emits a result, even for a failed turn.

A turn that never reached the engine and genuinely has no result can simply
drop the guard (warning included), which is the only zero-result path.

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
- **No consumer inside this repository.** The CLI does not yet construct a
  `MessageSink` — see the wiring note below. The crate compiles and is tested
  in isolation; it has no dependents.

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
  structural.
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

## Wiring the CLI (`--output-format stream-json`)

`--output-format` today accepts `text` (default) and `json`; `json` prints one
final blob from `crates/cli/src/orchestrator.rs`
(`format_headless_output`, ~line 1231). `stream-json` is not handled at all
today — it falls through to raw text. The change is:

1. **Construct the sink in `run_headless_with_prompt`**
   (`crates/cli/src/orchestrator.rs:1154`), right after the `ReplLoop` is built
   and before `repl.run_headless(...)`:
   `MessageSink::new(std::io::stdout())` when `ctx.output_format ==
   "stream-json"`, and hand it to the `ReplLoop`.
2. **The emitter has to live where `event_rx` is drained.** `ReplLoop` owns the
   engine event stream: the channel is created at
   `crates/cli/src/repl.rs:800` (`mpsc::unbounded_channel::<EngineEvent>()`),
   stored in the `event_rx` field (`repl.rs:522`), and read in `drain_events`
   (`repl.rs:2792`) and the TUI match loop (`repl.rs:2456`). One turn is one
   `run_headless` call (`repl.rs:2765`). So the `ReplLoop` needs an
   `Option<MessageSink<Stdout>>` field (set via a `with_protocol_sink`-style
   setter) and must map `EngineEvent` → `ProtocolMessage` where it already
   matches on engine events: `TextDelta` → `stream_event` (and/or an
   `assistant` frame), `ToolCallStart`/`ToolCallEnd` → an `assistant` `tool_use`
   block and the `user` `tool_result` frame, `CompactBoundary` → a `system`
   frame with `subtype: "compact_boundary"`, and `SessionEnd` → the turn's
   `result`. That translation is the only genuinely new code; constructing the
   sink and calling `begin_turn` / `emit_turn_complete` is small.
3. `format_headless_output` should skip its `json`/text formatting when
   `stream-json` is selected — the protocol frames are written to stdout as they
   happen, and `run_headless`'s returned string becomes `result.result` in the
   final `result` frame rather than something to `println!`.

`repl.rs` is not edited by this change; the above is the exact shape of the
wiring it needs.
