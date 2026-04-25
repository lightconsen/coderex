//! CodeRex SDK — structured I/O for headless/embedded usage.
//!
//! Provides a broadcast-based event stream, permission tracking, and user
//! elicitation suitable for servers, test rigs, and programmatic consumers.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use coderex_sdk::{SdkSession, SdkEvent};
//!
//! #[tokio::main]
//! async fn main() {
//!     let session = SdkSession::new("sess-1".into());
//!     let mut sub = session.subscribe_events();
//!
//!     session.emit(SdkEvent::TextDelta("Hello".into()));
//!
//!     let event = sub.recv().await.unwrap();
//!     assert!(matches!(event, SdkEvent::TextDelta(_)));
//! }
//! ```

#![warn(missing_docs)]

pub mod events;
pub mod store;

pub use events::{
    ArgType, ElicitationResponse, ElicitationTracker, PermissionSchema, PermissionStatus,
    PermissionTracker, PermissionValidationError, SdkEvent, SdkEventStream, SdkSession,
    SdkSubscriber, TrackedElicitation, TrackedPermission,
};
pub use store::{SessionStore, StoreError};
