//! CodeRex event stream — the line-delimited wire protocol and session state.
//!
//! # What this crate is
//!
//! [`protocol`] is a **wire format**: exactly one JSON object per line on an
//! output stream (`stdout`, for the CLI), with the `assistant` / `user` /
//! `system` / `result` / `stream_event` frames the reference uses. The
//! producer is the agent loop; every consumer is **out of process** — an SDK
//! client reading the stream, a desktop/IDE host driving the CLI, or the
//! transcript mirror. `result` is the turn-complete signal: exactly one per
//! turn.
//!
//! [`events`] carries the bookkeeping a session accumulates along the way:
//! permission-request tracking, user elicitation, and JSON persistence of that
//! state ([`store`]).
//!
//! # What this crate deliberately is not
//!
//! There is **no in-process event bus here, and there will not be**. This
//! crate once defined one (`EventBus` / `BusSubscriber` / `BusEvent`, a
//! `tokio::sync::broadcast` fan-out) modelled on the reference's message
//! hierarchy. The reference has no such bus — its `System`/`Assistant`/`User`/
//! `Result` taxonomy is a stream written to a pipe, consumed by processes that
//! are not this one. An in-process fan-out where nothing is subscribed is dead
//! code, and "wire up a subscriber" would have been the wrong repair. The
//! protocol's shape is one producer → one serialized output, so that is what
//! [`protocol::MessageSink`] is.
//!
//! The reference's *actual* in-process extension point is a hook registry over
//! engine events (`tool.call`, `ui.press`, `session.start`). That is a
//! different abstraction and is out of scope for this crate.
//!
//! # Where the socket protocol lives
//!
//! The WebSocket JSON protocol an out-of-process client speaks to the daemon
//! is `coderex-server` (`crates/server`); see `docs/server-protocol.md`. This
//! crate never opens a socket.
//!
//! # Emitting a turn
//!
//! ```
//! use coderex_events::protocol::{
//!     AssistantFrame, MessageBody, MessageSink, ProtocolMessage, ResultFrame, text_block,
//! };
//!
//! let mut sink = MessageSink::new(Vec::new());
//! let mut turn = sink.begin_turn("session-1");
//! turn.emit(&ProtocolMessage::Assistant(AssistantFrame {
//!     message: MessageBody::assistant(vec![text_block("hello")]),
//!     parent_tool_use_id: None,
//!     session_id: "sess-1".into(),
//! }))
//! .unwrap();
//! // The one and only turn-complete frame.
//! turn.emit_turn_complete(ResultFrame::success("sess-1", "hello", 12, 1))
//!     .unwrap();
//!
//! let lines = String::from_utf8(sink.into_inner()).unwrap();
//! assert_eq!(lines.lines().count(), 2);
//! ```

#![warn(missing_docs)]

pub mod events;
pub mod protocol;
pub mod store;

pub use events::{
    ArgType, ElicitationResponse, ElicitationTracker, PermissionSchema, PermissionStatus,
    PermissionTracker, PermissionValidationError, ResolvedTimeouts, SessionState,
    TrackedElicitation, TrackedPermission,
};
pub use protocol::{
    AssistantFrame, MessageBody, MessageSink, ProtocolMessage, ResultFrame, StdoutSink,
    StreamEventFrame, SystemFrame, Turn, TurnError, UserFrame, result_subtype, text_block,
    tool_result_block, tool_use_block,
};
pub use store::{SessionStore, StoreError};
