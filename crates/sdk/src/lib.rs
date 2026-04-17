//! CodeRex SDK — structured I/O for headless/embedded usage.

#![warn(missing_docs)]

pub mod events;

pub use events::{
    SdkEvent, SdkEventStream, SdkSession,
    PermissionStatus, TrackedPermission, PermissionTracker,
    ElicitationResponse, ElicitationTracker,
};
