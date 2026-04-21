# memory Module

## Goal

Session transcript storage, indexing, and long-term memory. Persists conversation history to disk, supports context compaction boundaries, provides AI-powered memory extraction and session fact tracking, team memory with checksum-based delta sync, and background session consolidation ("dream") — with optional encryption and HTTP transport.

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                        coderex-memory                            │
├──────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌─────────────┐  ┌──────────────────┐  ┌────────────────────┐  │
│  │ Transcript  │  │ SessionIndex     │  │ LongTermStore      │  │
│  │ Store       │  │ (SQLite + FTS5 + │  │ (hierarchical keys │  │
│  │ (JSONL      │  │  TF-IDF cosine)  │  │  on disk)          │  │
│  │  files)     │  │                  │  │                    │  │
│  └──────┬──────┘  └────────┬─────────┘  └────────┬───────────┘  │
│         │                  │                      │              │
│  ┌──────▼──────┐  ┌────────▼─────────┐  ┌────────▼───────────┐  │
│  │ Concurrent  │  │ session_memory   │  │ ai_workflow        │  │
│  │ wrappers    │  │ + session_memory │  │ + extract_memories │  │
│  │ (DashMap    │  │   _agent         │  │ (AI-powered        │  │
│  │  locking)   │  │ (session facts + │  │  memory ops)       │  │
│  │             │  │  update agent)   │  │                    │  │
│  └─────────────┘  └──────────────────┘  └────────────────────┘  │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │ team_memory (+ team_http)                                │   │
│  │ (checksum-based delta sync, tombstones, org-scoped, HTTP)│   │
│  └──────────────────────────────────────────────────────────┘   │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │ auto_dream                                               │   │
│  │ (threshold-based background consolidation, LLM optional) │   │
│  └──────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────┘
```

### Storage Layout

| Store | Format | Path |
|-------|--------|------|
| Transcripts | JSONL files | `~/.coderex/transcripts/{session_id}.jsonl` |
| Encrypted transcripts | JSONL + AES-256-GCM | `~/.coderex/transcripts/{session_id}.jsonl.enc` |
| Session index | SQLite + FTS5 + TF-IDF tables | `~/.coderex/sessions.db` |
| Long-term memories | JSON files | `~/.coderex/memories/{topic}.json` or `~/.coderex/memories/{key/path}.json` |
| Session facts | JSON files | `<base>/{session_uuid}/facts.json` |
| Team memories | JSON files | `<repo>/.coderex/team-memory/{org}/{topic}.json` |
| Dream state | JSON | `<memory_dir>/.dream_state.json` |
| Dream artifacts | Markdown | `<memory_dir>/dreams/{timestamp}.md` |

## Key Types

### Transcript Store (`transcript.rs`)

- **`TranscriptStore`** — Append-only JSONL transcript store. Each line is one JSON-serialized `Message`.
- **`TranscriptError`** — Error type covering I/O and JSON parse errors.
- **`PruneResult`** — Result of prune operation (archived, deleted, remaining counts).

Key methods: `append`, `read_all`, `read_all_skip_first_prompt`, `write_compact_boundary`, `truncate_before`, `rewrite`, `list_sessions`, `session_modified_at`, `archive_session` (gzip), `prune`, `backup`, `restore`.

Encryption (feature: `encryption`): `append_encrypted`, `read_all_decrypted` — AES-256-GCM with per-session keys, files use `.jsonl.enc` extension, each line is `nonce_hex:ciphertext_hex`.

### Session Index (`session_index.rs`)

- **`SessionIndex`** — SQLite metadata index with FTS5 full-text search and TF-IDF semantic search.
- **`SessionMeta`** — Session metadata (id, project_path, ai_title, tag, pr_url, cost, tokens).
- **`SessionIndexError`** — Error type for database operations.

Key methods: `open`, `create`, `update_last_active`, `update_cost_and_tokens`, `list_recent`, `list_by_project`, `get`, `search` (FTS5), `index_content` (TF-IDF), `search_semantic` (TF-IDF cosine similarity).

### Long-Term Store (`long_term.rs`)

- **`LongTermStore`** — File-based persistent memory with flat topics and hierarchical keys.
- **`MemoryEntry`** — A single memory (topic, optional key, content, timestamps, access_count).
- **`MemoryKey`** — Hierarchical slash-separated key (e.g. `project/backend/auth`). Supports prefix matching, parent navigation.

Key methods: `read`, `read_by_key`, `write`, `write_with_key`, `search` (case-insensitive substring, sorted by access_count), `read_by_prefix`, `list_keys`, `list_all`, `delete`, `delete_by_key`.

### Session Memory (`session_memory.rs`)

- **`SessionMemoryStore`** — In-memory session facts with disk persistence.
- **`SessionFact`** — A single fact (id, session_id, fact text, category, confidence).
- **`FactCategory`** — Enum: `ProjectStructure`, `CodePattern`, `Dependency`, `Architecture`, `UserPreference`, `Convention`, `Other`.

Key methods: `extract_fact`, `get_facts`, `get_facts_by_category`, `persist`, `load`.

### Team Memory (`team_memory.rs`)

- **`TeamMemorySyncService`** — Repo-scoped team memory with checksum-based delta sync.
- **`TeamMemoryEntry`** — Memory entry with topic, content, SHA-256 checksum, timestamps, tombstone flag.
- **`MemoryChecksum`** — SHA-256 hash for content comparison.
- **`SyncResult`** — Delta result: `to_send`, `to_receive`, counts.
- **`RemoteState`** — Remote server state representation.
- **`TeamSyncTransport`** — Trait for abstract network transport.
- **`InMemoryRelay`** — Test relay implementation.
- **`RelayTransport`** — Transport adapter for `InMemoryRelay`.

Key methods: `upsert`, `read`, `delete` (local tombstone), `list`, `compute_sync_delta`, `apply_remote`, `new_with_org` (org-scoped isolation).

### Auto Dream (`auto_dream.rs`)

- **`AutoDream`** — Background session consolidation manager.
- **`DreamConfig`** — Configuration (session_threshold, time_threshold_hours, enabled).
- **`DreamCheckResult`** — Threshold check result: `NotReady`, `ShouldDream`, `InProgress`.
- **`DreamSession`** — Dream session record (timestamps, sessions_analyzed, summary, success).

Key methods: `new`, `record_session`, `check_threshold`, `start_dream`, `complete_dream` (writes artifact to `dreams/`), `dream_history`, `generate_dream_prompt`, `spawn_watcher` (polling loop), `spawn_watcher_with_model` (LLM-powered via `dyn ApiClient`).

### Concurrent Wrappers (`concurrent.rs`)

- **`ConcurrentTranscriptStore`** — Thread-safe transcript store with per-session `DashMap` locking.
- **`ConcurrentLongTermStore`** — Thread-safe long-term store with per-topic `DashMap` locking.
- **`SharedSessionIndex`** — Type alias for `Arc<parking_lot::Mutex<SessionIndex>>`.

Key methods mirror the inner stores, each operation acquires the per-key mutex before delegating.

### HTTP Transport (`team_http.rs`, feature: `team-http`)

- **`HttpTransport`** — HTTP implementation of `TeamSyncTransport`.

Endpoints: `GET /state` (fetch checksums), `POST /delta` (send changes), `POST /entries` (fetch content by topic).

### Encryption Helpers (`transcript_crypto.rs`, feature: `encryption`)

- **`derive_cipher(master_key_hex, session_id)`** — HKDF-SHA256 per-session key derivation.
- **`generate_master_key()`** — Generate a random 32-byte master key as hex.

### AI Workflows (`ai_workflow.rs`, `extract_memories.rs`, `session_memory_agent.rs`, `prompts/mod.rs`)

- **`extract_long_term_memories_ai`** — Analyze conversation, extract and persist long-term memories via LLM.
- **`extract_single_memory_ai`** — Extract a single focused memory topic.
- **`update_session_memory_ai`** — Update session facts from recent conversation via LLM.
- **`session_memory_update_user_prompt`** — Assemble session memory prompt.
- **`extract_memories_user_prompt`** — Assemble long-term extraction prompt.
- **`MockApi`** — Test mock for `dyn ApiClient` (in `ai_workflow::tests`).

## Feature Flags

| Feature | Default | Dependencies | Description |
|---------|---------|--------------|-------------|
| `encryption` | off | `aes-gcm`, `hkdf`, `hex`, `rand_core` | AES-256-GCM encrypted transcripts |
| `team-http` | off | `reqwest` | HTTP transport for team memory sync |

Thread-safety (concurrent stores) and TF-IDF semantic search are always enabled (no feature flag).

## Architecture Patterns

- **JSONL append-only** — Each message is a JSON line; `sync_all()` after each write for durability.
- **Session-scoped files** — `~/.coderex/transcripts/{session_id}.jsonl`.
- **Compact boundary markers** — `<coderex_compact_boundary>` messages mark where context was compressed.
- **SQLite FTS5 + TF-IDF** — Full-text search over session metadata, TF-IDF cosine similarity over indexed content.
- **CRDT-style sync** — Team memory uses SHA-256 checksums and tombstones for conflict-free synchronization. Deletes are local-only and do not propagate.
- **Per-key concurrency** — `DashMap<String, Arc<Mutex<()>>>` for fine-grained locking; different keys proceed concurrently.
- **Feature-gated cryptography** — Encryption is opt-in via feature flag to keep default dependency graph small.
- **Atomic file writes** — Transcript rewrite uses temp file + `fs::rename` for crash safety.
- **Hierarchical memory keys** — `MemoryKey` with slash-separated segments maps to nested directory paths.

## Current Status

All modules are production-ready with full test coverage. No remaining gaps.

### What Works

- Transcript append, read, skip_first_prompt, corruption handling, compact boundaries.
- Transcript prune (archive to gzip + delete), backup/restore roundtrip.
- Session index: create, get, list (recent, by project), FTS5 search, TF-IDF semantic search.
- Long-term memory: flat topics, hierarchical keys, prefix reads, case-insensitive search, sorted by access_count.
- Session facts: in-memory storage with persist/load, category filtering.
- Team memory: upsert, delete (tombstone), list, checksum-based delta sync, org-scoped isolation, relay-based testing.
- Auto dream: threshold checking (session count + time), state persistence, background polling loop, LLM-powered mode, dream artifact generation.
- AI workflows: long-term memory extraction, single memory extraction, session memory update — all via `dyn ApiClient`.
- Thread-safe concurrent stores with per-key `DashMap` locking.
- Encrypted transcripts with AES-256-GCM + HKDF (feature: `encryption`).
- HTTP team memory transport (feature: `team-http`).
