# CodeRex Capability Matrix

## Legend

| Symbol | Meaning |
|--------|---------|
| ✅ | Implemented |
| 🔵 | Planned |

---

## Core Call Patterns

| Capability | Status | Notes |
|------------|--------|-------|
| Main agent streaming | ✅ | `QueryEngine::submit_streaming` |
| Side query (single-shot) | ✅ | `orchestrator::run_side_query` |
| Lightweight query | ✅ | `orchestrator::run_lightweight_query` |
| Forked agent | ✅ (库能力) | `orchestrator::run_forked_agent` — implemented and tested, but no production caller: sub-agents use the Agent tool, not this helper |
| Background feature | ✅ | `orchestrator::run_background_feature` |
| Writable flag for forked agents | ✅ | `BackgroundFeatureRequest::writable` |

## Prompt System

| Capability | Status | Notes |
|------------|--------|-------|
| Base prompt compiler | ✅ | `PromptCompiler` |
| Prompt profiles | ✅ | `PromptProfile` enum + registry |
| Model profiles | ✅ | `ModelProfile::Main/Lightweight/Background` |
| Mode prompt switching | ✅ | `/advisor`, `/bughunter`, `/reviewer` via commands |
| Feature-specific prompts | ✅ | `crates/core/src/prompts/features/*.rs` |
| Override system prompt | ✅ | `CommandResult::override_system_prompt` |
| Append system prompt | ✅ | `CommandResult::append_system_prompt` |

## AI Features

| Feature | Status | Call Pattern | Notes |
|---------|--------|--------------|-------|
| Session title | ✅ | Lightweight | `prompts::session_title` |
| Permission explanation | ✅ | Side query | `ExplainPermissionHook` |
| Compact summary | ✅ | Background | `/compact`, truncates via `engine.compact_session_messages(3)` |
| Session memory update | ✅ | Background | `/update-memory`, persists to `SessionMemoryStore` |
| Extract memories | ✅ | Background | `/extract-memories`, persists to `LongTermStore` |
| Prompt suggestion | ✅ | Background | Auto-triggered every 5 messages, rendered as ghost text in TUI input |
| Agent summary | ✅ | Background | `/agent-summary` |
| Away summary | ✅ | Background | `/away-summary` |
| Magic docs | ✅ | Background | `/magic-docs` |
| Codebase overview | ✅ | Background | `/codebase-overview` |
| README generation | ✅ | Background | `/generate-readme` |
| Agent generation | ✅ | Side query | `/agent-generation <description>` |

## Command System

| Capability | Status | Notes |
|------------|--------|-------|
| Slash commands | ✅ | `CommandRegistry` |
| Command effects (profile, tools, prompt) | ✅ | `CommandResult` |
| Non-interactive support | ✅ | `supports_non_interactive` flag |
| Remote-safe filtering | ✅ | `remote_safe` flag |
| Bridge-safe filtering | ✅ | `bridge_safe` flag |
| Agent-backend commands | ✅ | `uses_agent_backend` flag |

## Background Agent Infrastructure

| Capability | Status | Notes |
|------------|--------|-------|
| Observable tasks | ✅ | `BackgroundTask` with ID |
| Cancelable tasks | ✅ | `CancellationToken` |
| Result delivery | ✅ | `mpsc` channel to UI |
| Read-only enforcement | ✅ | `writable: false` filters tools |
| Task polling in TUI loop | ✅ | `poll_background_results()` |

## Memory System

| Capability | Status | Notes |
|------------|--------|-------|
| Session facts storage | ✅ | `SessionMemoryStore` |
| Long-term memory storage | ✅ | `LongTermStore` |
| AI-powered memory update | ✅ | `/update-memory`, writes facts + persists |
| AI-powered memory extraction | ✅ | `/extract-memories`, writes topics to store |
| Team memory sync | ✅ | `TeamMemorySyncService` with SHA-256 checksum delta sync, org scoping, tombstone deletes. Wired as `/team-memory <upsert|read|list|delete|sync>` |
| Auto-dream | ✅ | Threshold-based session consolidation with LLM integration. Wired as `/auto-dream <enable|disable|status>` |

## Agent Management

| Capability | Status | Notes |
|------------|--------|-------|
| Agent spawning | ✅ | `AgentManager::spawn_agent` |
| Context fork | ✅ | `ForkedContext` |
| Tool whitelist | ✅ | `SpawnOptions::allowed_tools` |
| Message channel | ✅ | `mpsc::UnboundedSender` |
| Lifecycle tracking | ✅ | `TaskManager` integration |
| Teammate events | ✅ | `TeammateEvent` callback |
| Resume support | — | Removed. Sub-agents are in-process tasks with no persisted transcript, so there is nothing to resume; `register_agent`/`resume_agent` no longer exist. |
| Forked agent execution | ✅ (库能力) | `orchestrator::run_forked_agent()` is implemented and tested but has no production caller; the product path is the Agent tool. The `AgentManager` wrapper was removed. |

## REPL Commands

| Command | Description |
|---------|-------------|
| `/compact` | Compact session context, truncate conversation messages |
| `/agent-summary` | Summarize recent agent activity |
| `/away-summary` | Summarize what happened while user was away |
| `/magic-docs` | Generate documentation for current codebase |
| `/codebase-overview` | Generate high-level codebase overview |
| `/generate-readme` | Generate project README |
| `/agent-generation <desc>` | Create a custom agent with description |
| `/update-memory` | Update session memory from recent conversation |
| `/extract-memories` | Extract long-term memories from conversation |
| `/auto-dream <enable|disable|status>` | Toggle background session consolidation |
| `/team-memory <sub>` | Upsert, read, list, delete, or sync team memories |
| `/advisor`, `/bughunter`, `/reviewer` | Switch prompt profiles |
