//! CodeRex Bridge system — remote session orchestration.

#![warn(missing_docs)]

pub mod archive;
pub mod delta_sync;
pub mod heartbeat;
pub mod orchestrator;
pub mod permission;
pub mod poller;
pub mod spawner;
pub mod state;
pub mod transport;
pub mod ws_transport;
