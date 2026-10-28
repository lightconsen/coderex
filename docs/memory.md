# memory Module

## Goal

Session transcript storage, indexing, and long-term memory. Persists conversation
history to disk, supports context compaction boundaries, provides AI-powered memory
extraction and session fact tracking, team memory with checksum-based delta sync, and
background session consolidation ("dream") — with optional encryption and HTTP
transport.

On top of that, the crate now carries the **model-facing memory surface**: three tools
(`memory_list`, `memory_read`, `memory_write`) over named stores, with optimistic
concurrency and a classifier gate in front of anything that leaves the machine.

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
│  │ Concurrent  │  │ session_memory   │  │ service + tools    │  │
│  │ wrappers    │  │ + session_memory │  │ (named stores,     │  │
│  │ (DashMap    │  │   _agent         │  │  if_version,       │  │
│  │  locking)   │  │ (session facts + │  │  classifier gate)  │  │
│  │             │  │  update agent)   │  │  + ai_workflow     │  │
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

## Where Data Lives

Everything lives under the workspace's own `.coderex/` directory. This is a local-first
tool: a transcript or a memory belongs to the workspace it came from, and it stays
next to it — nothing is hidden in `~/.coderex`, and nothing is written outside the
directory the user opened.

| Store | Format | Path |
|-------|--------|------|
| Transcripts | JSONL files | `<cwd>/.coderex/transcripts/{session_id}.jsonl` |
| Encrypted transcripts | JSONL + AES-256-GCM | `<cwd>/.coderex/transcripts/{session_id}.jsonl.enc` |
| Long-term memories | JSON files | `<cwd>/.coderex/memory/long_term/{topic}.json` or `{key/path}.json` |
| Session facts | JSON files | `<cwd>/.coderex/memory/session/{session_uuid}/facts.json` |
| Team memories | JSON files | `<cwd>/.coderex/memory/team/{topic}.json`, or `{org}/{topic}.json` when org-scoped |
| Dream state | JSON | `<cwd>/.coderex/memory/.dream_state.json` |
| Dream artifacts | Markdown | `<cwd>/.coderex/memory/dreams/{timestamp}.md` |
| Session index | SQLite + FTS5 + TF-IDF | `<cwd>/.coderex/sessions.db`; `session_resume::index_saved_session` writes it after transcript/snapshot persistence, and `search_sessions`/`list_recent_sessions` read it (with a transcript fallback). The exact `repl.rs` call sites are documented in `session_resume.rs`. |

The store roots are assembled in `crates/cli/src/repl.rs` (`memory_base` =
`<cwd>/.coderex/memory`, then `session/`, `long_term/`, `team/`); the transcript
directory comes from `crates/cli/src/session_resume.rs::transcript_dir`.

### The transcript path scheme, and why it is not Claude's

Claude Code writes `~/.claude/projects/<sanitized-cwd>/<sessionId>.jsonl`: every
project's transcripts are collected in one global directory, one subdirectory per
working directory. CodeRex writes `<cwd>/.coderex/transcripts/<sessionId>.jsonl`
instead, and that is deliberate rather than an oversight:

- **The workspace is the unit.** A transcript records what happened in this
  repository. Keeping it inside the repository means `rm -rf` on a checkout takes its
  transcripts with it, and a transcript cannot outlive the code it describes in a
  hidden directory nobody looks in.
- **No global state to clean up.** There is no accumulating `~/.coderex/projects/`
  tree to prune, and no hash-to-path mapping to maintain or explain.
- **`coderex-config` already has the other shape** — `DataDirectories::project_dir`
  returns `<base>/projects/<hash>` — and it is unused. Moving transcripts there would
  be a migration of user data for no gain, so it is left unused rather than
  half-adopted.

The trade-off is real and worth stating: two checkouts of the same repository have two
transcript directories, and moving a repository moves its transcripts. That is the
intended behaviour, not a bug.

### The session-id sanitization rule

The session id is the one part of the transcript path that comes from outside, so it
is checked before it is ever used as a file name. The rule, as implemented by
`transcript::sanitize_session_id`:

> A session id must be 1 to 128 characters long, and every character must be an ASCII
> letter, digit, `-`, or `_`. Anything else — empty, a path separator, `.` or `..`, a
> control character, a character outside ASCII — is refused.

Every `TranscriptStore` method applies it before touching the filesystem, including
`append`, `read_all`, `read_entries`, `rewrite`, `truncate_before`,
`write_compact_boundary`, `session_modified_at`, `archive_session`, `prune`, `backup`
and `restore`. A refused id returns `TranscriptError::InvalidSessionId` (or an
`InvalidInput` `io::Error` from the methods that report in `io::Result`) and no file is
created.

It is a **refusal, not a rewrite**: an id that does not satisfy the rule is reported
rather than mangled into one that does, because two ids that sanitize to the same file
name would be the same transcript. A UUID — which is what `SessionState` generates —
satisfies the rule unchanged.

## Key Types

### Transcript Entries (`entry.rs`)

A transcript line is a `TranscriptEntry`:

- **`TranscriptEntry::Message(Message)`** — a conversation message, serialized **bare**
  (`{"role": …, "content": …}`), exactly as every existing line is. Old files read
  unchanged.
- **`TranscriptEntry::CompactBoundary(CompactBoundary)`** — where context was
  compacted. The engine emits this today as a system message carrying
  `<coderex_compact_boundary>`; here it is a type instead of a magic string.
- **`TranscriptEntry::SessionMeta(SessionMeta)`** — a header naming the session id,
  working directory, model and start time, so a transcript can be understood after the
  file is moved.

Non-message entries carry a `"type"` discriminator; messages do not, which is what
keeps old files readable. `TranscriptEntry::from_message` classifies a message into the
right variant and `to_message` turns it back, so the two forms round-trip:

- `from_message` classifies a system message whose text starts with the marker as a
  `CompactBoundary`.
- `to_message` always yields a message the marker check recognizes — a note written
  without the marker is wrapped in it. The type and the string therefore agree in both
  directions.
- `SessionMeta` is metadata, not a message, so `to_message` returns `None` for it and
  `read_all` does not hand it to a message-only reader.

`EntryKind` names what an entry is at the granularity CodeRex actually produces:
`UserText`, `AssistantText`, `AssistantToolCall`, `ToolResult`, `SystemText`,
`CompactBoundary`, `SessionMeta`. `is_visible` says whether a person replaying the
session would see it (everything but the session header).

**Deliberately left out.** Claude's transcript format has three much larger tables:
nine visible message types, twenty-one system subtypes, and twenty-odd attachment
types. CodeRex writes none of the system subtypes — there are no hook-result records,
turn-duration records or API-error records in a transcript — and has no attachment
concept at all: a file the model read arrives as the output of a `Read` tool call
inside a `MessageContent::ToolResult`, not as a separate attachment entry. Modelling
those would describe a format CodeRex does not write. They earn a variant when CodeRex
grows the record that needs one.

**What the store writes.** `append`, `rewrite` and `truncate_before` write bare
messages, so the on-disk format is byte-for-byte what it was; `read_entries` classifies
on the way in, which is why a boundary written as a plain system message reads back as
a boundary. The tagged form is opt-in through `append_entry` (and
`write_compact_boundary`, which uses it). That keeps every reader that parses a line as
a bare `Message` — `coderex-insights`'s fact extraction, for one — working unchanged.

### Transcript Store (`transcript.rs`)

- **`TranscriptStore`** — Append-only JSONL transcript store, at
  `<base_path>/<session_id>.jsonl`.
- **`TranscriptError`** — Error type covering I/O, JSON parse, and an unusable session id.
- **`PruneResult`** — Result of prune operation (archived, deleted, remaining counts).
- **`sanitize_session_id`** — the session-id rule above.

Key methods: `append`, `append_entry`, `read_all`, `read_entries`,
`read_all_skip_first_prompt`, `write_compact_boundary`, `truncate_before`, `rewrite`,
`list_sessions`, `session_modified_at`, `archive_session` (gzip), `prune`, `backup`,
`restore`.

`read_all` and `read_all_skip_first_prompt` return `Vec<Message>` and drop non-message
entries; the skip rule is unchanged (the first system message is the system prompt).

Encryption (feature: `encryption`): `append_encrypted`, `read_all_decrypted` —
AES-256-GCM with per-session keys, files use `.jsonl.enc` extension, each line is
`nonce_hex:ciphertext_hex`.

### Session Index (`session_index.rs`)

- **`SessionIndex`** — SQLite metadata index with FTS5 full-text search and TF-IDF semantic search.
- **`SessionMeta`** — Session metadata (id, project_path, ai_title, tag, pr_url, cost, tokens).
- **`SessionIndexError`** — Error type for database operations.

Key methods: `open`, `create`, `upsert_with_content` (atomic create-or-update plus
FTS content), `update_last_active`, `update_cost_and_tokens`, `list_recent`,
`list_by_project`, `get`, `search` (FTS5), `index_content` (TF-IDF),
`search_semantic` (TF-IDF cosine similarity).

**CLI wiring surface.** `crates/cli/src/session_resume.rs` opens the workspace
index via `DataDirectories::new(<cwd>/.coderex).sessions_db_path()` and exposes
(the REPL call sites are documented in the module header):

- `index_saved_session(cwd, session_id, title, messages)` — called after
  transcript and interactive-snapshot persistence; delegates to
  `coderex_memory::session_backfill::index_transcript`, so re-indexing updates
  rather than duplicates and the backfill writes identical rows.
- `search_sessions(cwd, query, limit)` — FTS first, then a case-insensitive
  substring scan of `<cwd>/.coderex/transcripts/*.jsonl` for sessions the index
  does not know about.
- `list_recent_sessions(cwd, limit)` — merges index rows with transcript-only
  sessions, newest first.

### Backfill (`session_backfill.rs`)

- **`backfill(index, cwd, transcripts_dir)`** — writes a row for every
  `.jsonl` in the workspace's transcript directory, in one implementation shared
  with the CLI's save path (`index_transcript`). Titles come from the first user
  prompt, content from the parsed conversation, and a transcript that cannot be
  read is counted as skipped rather than failing the run.
- **`BackfillReport`** — transcripts seen / indexed / skipped, so a caller can
  say what happened.

`/backfill-sessions` is the production caller: the registry reports the
request, and the REPL performs the write inside `sync_command_state` (the same
place `/init` queues its hook) because only the CLI holds the workspace index
location.

Both read paths log a warning and fall back to transcripts when the database is
corrupt or locked; transcripts remain the source of truth for `/resume`, and the
index is an accelerator and search surface.

### Long-Term Store (`long_term.rs`)

- **`LongTermStore`** — File-based persistent memory with flat topics and hierarchical keys.
- **`MemoryEntry`** — A single memory (topic, optional key, content, timestamps, access_count, `version`).
- **`MemoryKey`** — Hierarchical slash-separated key (e.g. `project/backend/auth`). Supports prefix matching, parent navigation.

`version` is bumped on every write (`0` for entries written before the field existed, so
old files still load). It is what `if_version` pins; see below.

Key methods: `read`, `read_by_key`, `write`, `write_with_key`, `search` (case-insensitive substring, sorted by access_count), `read_by_prefix`, `list_keys`, `list_all`, `delete`, `delete_by_key`.

### Session Memory (`session_memory.rs`)

- **`SessionMemoryStore`** — In-memory session facts with disk persistence.
- **`SessionFact`** — A single fact (id, session_id, fact text, category, confidence).
- **`FactCategory`** — Enum: `ProjectStructure`, `CodePattern`, `Dependency`, `Architecture`, `UserPreference`, `Convention`, `Other`.

Key methods: `extract_fact`, `get_facts`, `get_facts_by_category`, `persist`, `load`.

### Team Memory (`team_memory.rs`)

- **`TeamMemorySyncService`** — Repo-scoped team memory with checksum-based delta sync.
- **`TeamMemoryEntry`** — Memory entry with topic, content, SHA-256 checksum, timestamps, tombstone flag, `version`.
- **`MemoryChecksum`** — SHA-256 hash for content comparison.
- **`SyncResult`** — Delta result: `to_send`, `to_receive`, counts.
- **`RemoteState`** — Remote server state representation.
- **`TeamSyncTransport`** — Trait for abstract network transport.
- **`InMemoryRelay`** — Test relay implementation.
- **`RelayTransport`** — Transport adapter for `InMemoryRelay`.

Key methods: `upsert`, `read`, `delete` (local tombstone), `list`, `compute_sync_delta`, `apply_remote`, `new_with_org` (org-scoped isolation).

### The Model-Facing Memory Surface (`service.rs`, `tools.rs`)

Claude exposes `memory_list` / `memory_read` / `memory_write` over several stores, with
`if_version` optimistic concurrency, and requires a classifier to pass before a shared
store is written. This is CodeRex's equivalent. It owns no storage of its own: it
routes to the stores that already exist and adds the three things they lack — a store
name a model can say, a version to pin, and a gate for writes that leave the machine.

| Model store | Backing store | Key form |
|---|---|---|
| `personal` | `LongTermStore`, `personal/` namespace | hierarchical |
| `project` | `LongTermStore`, `project/` namespace | hierarchical |
| `shared` | `TeamMemorySyncService` | flat topic |
| `org` | a second, org-scoped `TeamMemorySyncService` | flat topic |

`personal` and `project` share one `LongTermStore` and are told apart by a key
namespace. That follows from the local-first layout above: there is no hidden
`~/.coderex` memory directory, so a "personal" memory is still written next to the
workspace it belongs to. The two namespaces keep the two scopes from colliding; they do
not promise a global one.

`SessionMemoryStore` is **not** a model store. Its facts are extracted from a session by
the engine, are keyed by session id, and are not addressed by a model-chosen key —
exposing it as `memory_write` would mean inventing keys the store does not have.

**Key shape follows the store.** `personal` and `project` key by path and take
hierarchical keys (`backend/auth`); `shared` and `org` key by topic and take a flat
one. A hierarchical key addressed to a synced store is refused rather than accepted:
the store writes `<topic>.json` with `/`, `\`, `:` and space replaced by `_`, so
`backend/auth` and `backend_auth` would be the same file, and silently merging two keys
is worse than refusing the first.

- **`MemoryStore`** — which store a call is addressed to: `Personal`, `Project`,
  `Shared`, `Org`. `parse`, `as_str`, `all`, `requires_classification`.
- **`MemoryRecord`** — one memory as the service sees it (store, key, content, version,
  created_at, updated_at).
- **`MemoryService`** — `list`, `read`, `write(store, key, content, if_version)`,
  `delete(store, key, if_version)`. Built over the stores the REPL already constructs:
  `MemoryService::new(long_term, team)`, `.with_org(...)`, `.with_classifier(...)`.
- **`MemoryServiceError`** — `NotFound`, `VersionMismatch { expected, found }`,
  `Refused { reason }`, `StoreUnavailable`, `Invalid`, `Io`.
- **`MemoryWriteClassifier`** — `async fn classify(store, key, content) -> Result<(), String>`.
  `AllowAllClassifier` is the explicit opt-out.
- **`MemoryTools`** — the family: `new(service)`, `all()`, `tool(name)`, `handles(name)`.
- **`MemoryTool`** — one tool: `name`, `description`, `input_schema`, `output_schema`,
  `is_read_only`, and `async fn call(&Value) -> Result<String, MemoryToolError>`.

**Versioning.** Every entry carries a `version`, bumped on each write. A write may pin
the version it expects (`if_version`); if the entry has moved on, the write is refused
and reports the version that is there now, so a caller re-reads instead of clobbering a
change it never saw. `memory_read` reports the version in its header, which is what a
later `memory_write` pins. This is the same convention the artifact database uses
(`coderex-artifacts`'s `db::Write`), reused rather than reinvented.

**The classifier gate.** A write to `shared` or `org` leaves the machine, so it must
pass a `MemoryWriteClassifier` first. When no classifier is configured the write is
**refused**, not waved through: a shared write that nobody classified is exactly the
write the gate exists to stop. A caller that really wants the gate open says so with
`AllowAllClassifier` — a named, greppable choice rather than a silent skip. The hook
shape is the permission engine's (`AutoClassifierHook` in `coderex-core`).

**Why the tools are not `impl coderex_tools::Tool`.** They cannot be. `coderex-tools`
depends on `coderex-memory` (its `BriefTool` reads transcripts), so the reverse
dependency would close a cycle, and Cargo rejects a cycle even when the dependency is
optional. The family is therefore written against plain JSON — the shape the `Tool`
trait passes around, with no reference to it — and a crate that depends on both wraps
it:

```rust
// In a crate that depends on both coderex-memory and coderex-tools.
impl coderex_tools::Tool for MemoryToolAdapter {
    fn descriptor(&self) -> coderex_tools::ToolDescriptor {
        coderex_tools::ToolDescriptor {
            name: self.tool.name().to_string(),
            tier: coderex_tools::ToolTier::Baseline,
            input_schema: self.tool.input_schema(),
            output_schema: self.tool.output_schema(),
            requires_approval: false,
            is_enabled: true,
            is_read_only: self.tool.is_read_only(),
            is_concurrency_safe: self.tool.is_read_only(),
            interrupt_behavior: coderex_tools::InterruptBehavior::RunToCompletion,
        }
    }

    async fn execute(
        &self,
        input: coderex_tools::ToolInput,
        _ctx: coderex_tools::ToolContext<'_>,
    ) -> Result<coderex_tools::ToolOutput, coderex_tools::ToolError> {
        self.tool
            .call(&input.arguments)
            .await
            .map(|output| coderex_tools::ToolOutput { output, is_display: true })
            .map_err(|error| coderex_tools::ToolError::InvalidInput {
                message: error.to_string(),
            })
    }
}
```

Registering the family is then the same shape the artifact family uses:

```rust
for tool in coderex_memory::tools::MemoryTools::new(memory_service).all() {
    dispatcher
        .registry()
        .register_dynamic(std::sync::Arc::new(MemoryToolAdapter::new(tool)));
}
```

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
- **Local-first paths** — transcripts and memories live under the workspace's `.coderex/`, never in a home-directory tree.
- **Sanitized session ids** — the one externally-supplied path component is checked, not escaped.
- **Classified entries** — a bare message line is read as the entry it is, so the type and the marker string cannot disagree.
- **Compact boundary markers** — `<coderex_compact_boundary>` messages mark where context was compressed; they are `CompactBoundary` entries on the way in and out.
- **Optimistic concurrency** — `version` + `if_version` on long-term and team entries; a stale pin is refused with the version that is there now.
- **Fail-closed shared writes** — a write that leaves the machine without a classifier is refused, not allowed.
- **SQLite FTS5 + TF-IDF** — Full-text search over session metadata, TF-IDF cosine similarity over indexed content.
- **CRDT-style sync** — Team memory uses SHA-256 checksums and tombstones for conflict-free synchronization. Deletes are local-only and do not propagate.
- **Per-key concurrency** — `DashMap<String, Arc<Mutex<()>>>` for fine-grained locking; different keys proceed concurrently.
- **Feature-gated cryptography** — Encryption is opt-in via feature flag to keep default dependency graph small.
- **Atomic file writes** — Transcript rewrite uses temp file + `fs::rename` for crash safety.
- **Hierarchical memory keys** — `MemoryKey` with slash-separated segments maps to nested directory paths.

## Current Status

### What Works

- Transcript append, read, entry-typed read, skip_first_prompt, corruption handling, compact boundaries.
- Session-id sanitization on every transcript method.
- Transcript prune (archive to gzip + delete), backup/restore roundtrip.
- Session index: create, atomic create-or-update with content, get, list (recent, by project), FTS5 search, TF-IDF semantic search.
- Session index CLI surface: `index_saved_session` after persistence, and `search_sessions` / `list_recent_sessions` for `/resume` search with transcript fallback; exact `repl.rs` call sites are documented in `session_resume.rs`.
- Long-term memory: flat topics, hierarchical keys, prefix reads, case-insensitive search, sorted by access_count, `version` on every entry.
- Session facts: in-memory storage with persist/load, category filtering.
- Team memory: upsert, delete (tombstone), list, checksum-based delta sync, org-scoped isolation, relay-based testing.
- Model-facing memory: four named stores, `if_version` pinning, a fail-closed classifier gate, and the three-tool family.
- Auto dream: threshold checking (session count + time), state persistence, background polling loop, LLM-powered mode, dream artifact generation.
- AI workflows: long-term memory extraction, single memory extraction, session memory update — all via `dyn ApiClient`.
- Thread-safe concurrent stores with per-key `DashMap` locking.
- Encrypted transcripts with AES-256-GCM + HKDF (feature: `encryption`).
- HTTP team memory transport (feature: `team-http`).

### What Is Not Wired

- **The memory tools are registered.** `crates/cli/src/memory_tools.rs` adapts
  the family to `coderex_tools::Tool`, and `crates/cli/src/repl.rs` registers it,
  so `memory_list`, `memory_read` and `memory_write` are visible to the model.
  The `org` store is only enabled when an org id is configured
  (`CODEREX_ORG_ID`, else `org_id` in the orchestrator's
  `remote_settings_cache.json`); without one, `org` reports `StoreUnavailable`
  instead of silently resolving to another store.
- **`SessionIndex` is exposed as an accelerator, not a source of truth.**
  `session_resume` owns the write (`index_saved_session`) and read
  (`search_sessions` / `list_recent_sessions`) surface. The REPL indexes after
  each persisted turn (`persist_interactive_snapshot`) and after every headless
  turn; `/resume` resolves a title fragment through the index (an ambiguous
  fragment lists candidates). The index is opened once per workspace and shared
  as a `SharedSessionIndex`; `index_content` fills the TF-IDF table, and a
  query with no FTS hit falls back to `search_semantic` before scanning
  transcripts.
  Exact-id resume still reads the transcript, and every index read tolerates a
  corrupt or locked database by warning and scanning transcripts.
- **`DataDirectories` (`coderex-config`) is only partly adopted.**
  `sessions_db_path` is used for the workspace-local `<cwd>/.coderex/sessions.db`
  by rooting a `DataDirectories` at `<cwd>/.coderex`; `project_dir` and
  `ensure_all` still describe a home-directory layout that nothing uses.
