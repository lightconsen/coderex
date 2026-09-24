//! CodeRex memory system — transcript storage, session indexing, long-term memory.

#![warn(missing_docs)]

pub mod ai_workflow;
pub mod auto_dream;
pub mod concurrent;
pub mod extract_memories;
pub mod long_term;
pub mod prompts;
pub mod session_index;
pub mod session_memory;
pub mod session_memory_agent;
pub mod session_memory_backend;
pub mod team_memory;
pub mod transcript;

#[cfg(feature = "encryption")]
pub mod transcript_crypto;

#[cfg(feature = "team-http")]
pub mod team_http;

pub use auto_dream::{AutoDream, DreamCheckResult, DreamConfig, DreamSession};
pub use concurrent::{ConcurrentLongTermStore, ConcurrentTranscriptStore, SharedSessionIndex};
pub use long_term::{LongTermStore, MemoryEntry, MemoryKey};
pub use team_memory::{
    MemoryChecksum, RemoteState, SyncResult, TeamMemoryEntry, TeamMemorySyncService,
    TeamSyncTransport,
};

#[cfg(feature = "encryption")]
pub use transcript_crypto::{derive_cipher, generate_master_key};

#[cfg(feature = "team-http")]
pub use team_http::HttpTransport;
