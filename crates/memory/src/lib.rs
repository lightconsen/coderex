//! CodeRex memory system — transcript storage, session indexing, long-term memory.

#![warn(missing_docs)]

pub mod transcript;
pub mod session_index;
pub mod long_term;
pub mod session_memory;
pub mod team_memory;
pub mod auto_dream;
pub mod ai_workflow;
pub mod session_memory_agent;
pub mod extract_memories;
pub mod prompts;
pub mod concurrent;

#[cfg(feature = "encryption")]
pub mod transcript_crypto;

#[cfg(feature = "team-http")]
pub mod team_http;

pub use long_term::{LongTermStore, MemoryEntry, MemoryKey};
pub use team_memory::{
    TeamMemoryEntry, TeamMemorySyncService, MemoryChecksum,
    SyncResult, RemoteState, TeamSyncTransport,
};
pub use auto_dream::{
    AutoDream, DreamConfig, DreamCheckResult, DreamSession,
};
pub use concurrent::{ConcurrentTranscriptStore, ConcurrentLongTermStore, SharedSessionIndex};

#[cfg(feature = "encryption")]
pub use transcript_crypto::{derive_cipher, generate_master_key};

#[cfg(feature = "team-http")]
pub use team_http::HttpTransport;
