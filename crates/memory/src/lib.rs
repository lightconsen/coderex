//! CodeRex memory system — transcript storage, session indexing, long-term memory.

#![warn(missing_docs)]

pub mod transcript;
pub mod session_index;
pub mod long_term;
pub mod session_memory;
pub mod team_memory;
pub mod auto_dream;

pub use team_memory::{
    TeamMemoryEntry, TeamMemorySyncService, MemoryChecksum,
    SyncResult, RemoteState,
};
pub use auto_dream::{
    AutoDream, DreamConfig, DreamCheckResult, DreamSession,
};
