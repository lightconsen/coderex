//! CodeRex daemon server — WebSocket sessions and SDK structured I/O.

#![warn(missing_docs)]

pub mod session;
pub mod filter;
pub mod persistence;
pub mod control_plane;

pub use session::{Session, ServerConfig, DaemonServer, WsRequest, WsRequestKind, WsResponse, WsResponseKind, WsEvent};
pub use filter::{CommandFilter, SafetyLevel};
pub use persistence::{SessionStore, PersistedSession};
pub use control_plane::{ControlPlaneServer, ControlPlaneState, RegisterRequest, RegisterResponse, AssignmentEntry, CapacityReport};
