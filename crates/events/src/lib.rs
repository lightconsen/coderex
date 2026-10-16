//! CodeRex event bus — in-process, session-scoped event fan-out.
//!
//! This crate is **not** an external protocol or a client SDK. It is the
//! in-process event bus the CLI uses to fan a session's `EngineEvent`s out to
//! any in-process subscriber, plus the bookkeeping that goes with a session:
//! permission-request tracking, user elicitation, and JSON persistence of that
//! state.
//!
//! # Where the external protocol lives
//!
//! If you are looking for the structured I/O that an *out-of-process* client
//! speaks, this is not it. CodeRex's external protocol is the WebSocket JSON
//! protocol served by the `coderex-server` crate (`crates/server`); see
//! `docs/server-protocol.md` for the wire format and `docs/server.md` for the
//! server itself. This crate never touches a socket — it is a
//! `tokio::sync::broadcast` channel (capacity 4096) that lives inside a single
//! process.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use coderex_events::{EventSession, BusEvent};
//!
//! #[tokio::main]
//! async fn main() {
//!     let session = EventSession::new("sess-1".into());
//!     let mut sub = session.subscribe_events();
//!
//!     session.emit(BusEvent::TextDelta("Hello".into()));
//!
//!     let event = sub.recv().await.unwrap();
//!     assert!(matches!(event, BusEvent::TextDelta(_)));
//! }
//! ```

#![warn(missing_docs)]

pub mod events;
pub mod store;

pub use events::{
    ArgType, BusEvent, BusSubscriber, ElicitationResponse, ElicitationTracker, EventBus,
    EventSession, PermissionSchema, PermissionStatus, PermissionTracker, PermissionValidationError,
    TrackedElicitation, TrackedPermission,
};
pub use store::{SessionStore, StoreError};
