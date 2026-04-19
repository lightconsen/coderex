# memory Module Inspection

## Goal

Session transcript storage, indexing, and long-term memory. Persists conversation history to disk, supports context compaction boundaries, and provides hooks for future vector-based retrieval.

## Design

### Key Types

- **`TranscriptStore`** (`transcript.rs`) — File-based JSONL store. Append-only writes, full-read for sessions.
- **`SessionIndex`** (`session_index.rs`) — Indexes sessions by metadata for fast listing.
- **`LongTermMemory`** (`long_term.rs`) — Placeholder for vector-based semantic search.
- **`SessionMemory`** (`session_memory.rs`) — In-memory session cache with compaction awareness.
- **`TeamMemory`** (`team_memory.rs`) — Collaborative memory with sync service and checksums.
- **`AutoDream`** (`auto_dream.rs`) — Background summarization agent for long sessions.

### Architecture Patterns

- **JSONL append-only** — Each message is a JSON line; atomic append for durability.
- **Session-scoped files** — `~/.coderex/transcripts/{session_id}.jsonl`.
- **Compact boundary markers** — Special `<coderex_compact_boundary>` messages indicate context windows that were compressed.

## Current Status

### What Works

- TranscriptStore append and read_all operations.
- JSONL persistence with directory creation.
- Session index for listing available transcripts.
- Compact boundary detection and counting.
- Team memory sync types and checksum validation.
- AutoDream configuration and session types.

### What Is Stubbed/Partial

- **LongTermMemory** — Types exist but no vector DB integration (no embeddings, no similarity search).
- **AutoDream** — Configuration types only; no actual background summarization running.
- **TeamMemory sync** — Network sync service is typed but not wired to actual sync protocol.

## Gaps for Production

- **No embeddings/semantic search**: Cannot find "that message about authentication from last week".
  → **Suggestion**: Integrate `fastembed-rs` or `ort` for local embeddings. Store vectors in an SQLite extension (`sqlite-vec`) or `lance` dataset. Index on session boundaries and query with cosine similarity.
- **No transcript encryption**: Session files are plaintext JSONL.
  → **Suggestion**: Encrypt transcripts at rest using `aes-gcm` with a key derived from the user's OS login (via `keyring`). Store the nonce per file header. Decrypt transparently on read.
- **No transcript pruning policy**: Files grow unbounded; no automatic archival.
  → **Suggestion**: Add a retention policy config (e.g., `keep_last: 30_sessions`). On startup, archive sessions older than the threshold to a compressed `.tar.gz` in `~/.coderex/archives/`. Delete originals after successful archive.
- **No cross-session search**: Can only read one session at a time.
  → **Suggestion**: Maintain an inverted index (word → [(session_id, line_number)]) in SQLite. Update it incrementally on each transcript append. Provide a `/search` command that queries the index.
- **Team memory sync unimplemented**: Types defined but no actual network sync.
  → **Suggestion**: Implement a simple CRDT-based sync over WebSocket. Each team member's session appends to a shared Merkle tree. Resolve conflicts by timestamp. Start with a central relay server.
- **Session indexing is basic**: No full-text index, just file listing.
  → **Suggestion**: Build an FTS5 virtual table in SQLite over transcript content. Index each message as a row with `session_id`, `timestamp`, `role`, `content`. Query with MATCH syntax.
- **No backup/restore**: Single file per session; corruption loses entire session.
  → **Suggestion**: After every N appends (e.g., 10), create a `.jsonl.bak` copy. On read failure, attempt to restore from the backup. Also support manual `/backup` and `/restore` commands.
