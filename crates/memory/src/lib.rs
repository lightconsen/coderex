//! CodeRex memory system — transcript storage, session indexing, long-term memory.

#![warn(missing_docs)]

pub mod ai_workflow;
pub mod auto_dream;
pub mod concurrent;
pub mod entry;
pub mod extract_memories;
pub mod long_term;
pub mod prompts;
pub mod service;
pub mod session_backfill;
pub mod session_index;
pub mod session_memory;
pub mod session_memory_agent;
pub mod session_memory_backend;
pub mod team_memory;
pub mod tools;
pub mod transcript;

#[cfg(feature = "encryption")]
pub mod transcript_crypto;

#[cfg(feature = "team-http")]
pub mod team_http;

pub use auto_dream::{AutoDream, DreamCheckResult, DreamConfig, DreamSession};
pub use concurrent::{ConcurrentLongTermStore, ConcurrentTranscriptStore, SharedSessionIndex};
pub use entry::{EntryKind, TaggedEntry, TranscriptEntry};
pub use long_term::{LongTermStore, MemoryEntry, MemoryKey};
pub use service::{
    AllowAllClassifier, MemoryRecord, MemoryService, MemoryServiceError, MemoryStore,
    MemoryWriteClassifier,
};
pub use session_backfill::{
    BackfillReport, MAX_INDEXED_CONTENT_BYTES, backfill, index_transcript, session_content,
    transcript_dir, transcript_title,
};
pub use team_memory::{
    MemoryChecksum, RemoteState, SyncResult, TeamMemoryEntry, TeamMemorySyncService,
    TeamSyncTransport,
};
pub use tools::{MemoryTool, MemoryToolError, MemoryTools};
pub use transcript::{TranscriptError, TranscriptStore, sanitize_session_id};

#[cfg(feature = "encryption")]
pub use transcript_crypto::{derive_cipher, generate_master_key};

#[cfg(feature = "team-http")]
pub use team_http::HttpTransport;
