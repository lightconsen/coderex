# memory Module Inspection

## Goal

Session transcript storage, indexing, and long-term memory. Persists conversation history to disk, supports context compaction boundaries, and provides hooks for future vector-based retrieval.

## Design

### Key Types

- **`TranscriptStore`** (`transcript.rs`) — File-based JSONL store. Append-only writes, full-read for sessions.
- **`SessionIndex`** (`session_index.rs`) — SQLite metadata index with FTS5 full-text search.
- **`LongTermStore`** (`long_term.rs`) — Topic-based structured memory with file-backed JSON persistence.
- **`SessionMemory`** (`session_memory.rs`) — In-memory session cache with compaction awareness.
- **`TeamMemorySyncService`** (`team_memory.rs`) — Collaborative memory with file-based sync, checksums, and org-scoped isolation.
- **`AutoDream`** (`auto_dream.rs`) — Background session consolidation triggered by configurable thresholds.

### Architecture Patterns

- **JSONL append-only** — Each message is a JSON line; atomic append for durability.
- **Session-scoped files** — `~/.coderex/transcripts/{session_id}.jsonl`.
- **Compact boundary markers** — Special `<coderex_compact_boundary>` messages indicate context windows that were compressed.
- **SQLite FTS5** — Full-text search over session metadata (titles, tags) via a virtual table.
- **CRDT-style sync** — Team memory uses checksums and tombstones for conflict-free synchronization.

## Current Status

### What Works

- TranscriptStore append, read_all, and message filtering (skip_first_prompt).
- JSONL persistence with directory creation and corrupted line skipping.
- Session index for listing, filtering by project, and FTS5 full-text search.
- Compact boundary detection, counting, and truncation.
- Transcript pruning with automatic archival to gzip-compressed files.
- Transcript backup (`.jsonl.bak`) and restore roundtrip.
- Team memory sync service with upsert, delete (tombstone), list, and org-scoped isolation.
- Team memory sync delta computation and remote entry application.
- Team memory transport abstraction (`TeamSyncTransport` trait) with `InMemoryRelay` for testing.
- AutoDream configuration, threshold checking, session counting, and state persistence.
- AutoDream background polling loop (`spawn_watcher`) with shutdown signal.
- LongTermMemory topic-based storage with delete, delete_by_key, and list_all operations.

### Remaining Gaps for Production

- **No embeddings/semantic search**: Cannot find "that message about authentication from last week" across transcript content.
  → **Suggestion**: Integrate `fastembed-rs` or `ort` for local embeddings. Store vectors in `sqlite-vec` or `lance` dataset. Index on session boundaries and query with cosine similarity.
- **No transcript encryption at rest**: Session files are plaintext JSONL.
  → **Suggestion**: Encrypt transcripts using `aes-gcm` with a key derived from the user's OS login (via `keyring`). Store the nonce per file header. Decrypt transparently on read.
- **Team memory uses file-based sync only**: Types for network transport (`TeamSyncTransport` trait) exist but no HTTP/WebSocket implementation is wired up.
  → **Suggestion**: Implement `TeamSyncTransport` over HTTP with the existing relay pattern. Each team member's session sends deltas to a central relay. Resolve conflicts by timestamp with last-write-wins semantics.
- **AutoDream spawns locally**: The dream agent runs synchronously on the same process; no external LLM sub-agent is actually invoked.
  → **Suggestion**: Wire `spawn_watcher` to spawn a read-only sub-agent that receives session summaries via the channel and produces structured analysis output. The `generate_dream_prompt()` method already provides the prompt template.
