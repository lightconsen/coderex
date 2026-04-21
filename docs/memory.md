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
- **`HttpTransport`** (`team_http.rs`) — HTTP implementation of `TeamSyncTransport` for relay-based team sync.
- **`AutoDream`** (`auto_dream.rs`) — Background session consolidation triggered by configurable thresholds.
- **`ConcurrentTranscriptStore`** / **`ConcurrentLongTermStore`** (`concurrent.rs`) — Thread-safe wrappers with per-key `DashMap` locking.

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
- Thread-safe concurrent appends via `ConcurrentTranscriptStore` with per-session `DashMap` locking.
- TF-IDF semantic search on session content via `SessionIndex::index_content` / `search_semantic`.
- Encrypted transcript append/decrypt with AES-256-GCM + HKDF key derivation (feature: `encryption`).
- HTTP team memory sync via `HttpTransport` (feature: `team-http`).
- AutoDream LLM sub-agent via `spawn_watcher_with_model` accepting `dyn ApiClient`.

### Closed Gaps

All previously identified production gaps have been implemented:

- **Thread-safety on shared stores**: `ConcurrentTranscriptStore` and `ConcurrentLongTermStore` wrap stores with per-key `DashMap<String, Arc<Mutex<()>>>` locking. `SharedSessionIndex` wraps `SessionIndex` in `Arc<parking_lot::Mutex<>>`. (module: `concurrent.rs`)
- **Embeddings/semantic search**: TF-IDF cosine similarity search on `SessionIndex` via `index_content()` and `search_semantic()` methods. Stores term frequencies in `session_tfidf`/`session_idf` tables. (module: `session_index.rs`)
- **Transcript encryption at rest**: `append_encrypted()` / `read_all_decrypted()` methods using AES-256-GCM with per-session keys derived via HKDF. Encrypted files use `.jsonl.enc` extension. (feature: `encryption`, module: `transcript_crypto.rs`)
- **Team memory HTTP transport**: `HttpTransport` implements `TeamSyncTransport` over HTTP/JSON using `reqwest`. Endpoints: `GET /state`, `POST /delta`, `POST /entries`. (feature: `team-http`, module: `team_http.rs`)
- **AutoDream LLM sub-agent**: `spawn_watcher_with_model()` accepts `&dyn ApiClient` to send dream prompts to an external LLM and parse the response. (module: `auto_dream.rs`)
