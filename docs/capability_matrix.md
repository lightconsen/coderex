# CodeRex Capability Matrix

Alignment target: `cc-src` (Claude Code source) AI calling patterns.

## Legend

| Symbol | Meaning |
|--------|---------|
| ✅ | Implemented |
| 🟡 | Partial / Stubbed |
| ❌ | Not implemented |
| 🔵 | Planned |

---

## Core Call Patterns

| Capability | cc-src | coderex | Notes |
|------------|--------|---------|-------|
| Main agent streaming | ✅ | ✅ | `QueryEngine::submit_streaming` |
| Side query (single-shot) | ✅ | ✅ | `orchestrator::run_side_query` |
| Lightweight query | ✅ | ✅ | `orchestrator::run_lightweight_query` |
| Forked agent | ✅ | ✅ | `orchestrator::run_forked_agent` |
| Background feature | ✅ | ✅ | `orchestrator::run_background_feature` |
| Writable flag for forked agents | ✅ | ✅ | `BackgroundFeatureRequest::writable` |

## Prompt System

| Capability | cc-src | coderex | Notes |
|------------|--------|---------|-------|
| Base prompt compiler | ✅ | ✅ | `PromptCompiler` |
| Prompt profiles | ✅ | ✅ | `PromptProfile` enum + registry |
| Model profiles | ✅ | ✅ | `ModelProfile::Main/Lightweight/Background` |
| Mode prompt switching | ✅ | ✅ | `/advisor`, `/bughunter`, `/reviewer` via commands |
| Feature-specific prompts | ✅ | ✅ | `crates/core/src/prompts/*.rs` |
| Override system prompt | ✅ | ✅ | `CommandResult::override_system_prompt` |
| Append system prompt | ✅ | ✅ | `CommandResult::append_system_prompt` |

## AI Features

| Feature | cc-src | coderex | Call Pattern | Notes |
|---------|--------|---------|--------------|-------|
| Session title | ✅ | ✅ | Lightweight | `prompts::session_title` |
| Permission explanation | ✅ | ✅ | Side query | `ExplainPermissionHook` |
| Compact summary | ✅ | 🟡 | Background | Prompt exists, wired as `/compact` |
| Session memory update | ✅ | 🟡 | Background | Prompt exists, wired as `/update-memory` |
| Extract memories | ✅ | 🟡 | Background | Prompt exists, wired as `/extract-memories` |
| Prompt suggestion | ✅ | ✅ | Background | Auto-triggered in REPL |
| Agent summary | ✅ | 🟡 | Background | Prompt exists, wired as `/agent-summary` |
| Away summary | ✅ | 🟡 | Background | Prompt exists, wired as `/compact` |
| Magic docs | ✅ | 🟡 | Background | Prompt exists, wired as `/magic-docs` |
| Codebase overview | ✅ | 🟡 | Background | Prompt exists, not wired to command |
| README generation | ✅ | 🟡 | Background | Prompt exists, wired as `/generate-readme` |
| Agent generation | ✅ | 🟡 | Side query | Prompt exists, wired as `/agent-generation` |

## Command System

| Capability | cc-src | coderex | Notes |
|------------|--------|---------|-------|
| Slash commands | ✅ | ✅ | `CommandRegistry` |
| Command effects (profile, tools, prompt) | ✅ | ✅ | `CommandResult` |
| Non-interactive support | ✅ | ✅ | `supports_non_interactive` flag |
| Remote-safe filtering | ✅ | ✅ | `remote_safe` flag |
| Bridge-safe filtering | ✅ | ✅ | `bridge_safe` flag |
| Agent-backend commands | ✅ | ✅ | `uses_agent_backend` flag |

## Background Agent Infrastructure

| Capability | cc-src | coderex | Notes |
|------------|--------|---------|-------|
| Observable tasks | ✅ | ✅ | `BackgroundTask` with ID |
| Cancelable tasks | ✅ | ✅ | `CancellationToken` |
| Result delivery | ✅ | ✅ | `mpsc` channel to UI |
| Read-only enforcement | ✅ | ✅ | `writable: false` filters tools |
| Task polling in TUI loop | ✅ | ✅ | `poll_background_results()` |

## Memory System

| Capability | cc-src | coderex | Notes |
|------------|--------|---------|-------|
| Session facts storage | ✅ | ✅ | `SessionMemoryStore` |
| Long-term memory storage | ✅ | ✅ | `LongTermStore` |
| AI-powered memory update | ✅ | 🟡 | `ai_workflow::update_session_memory_ai` |
| AI-powered memory extraction | ✅ | 🟡 | `ai_workflow::extract_long_term_memories_ai` |
| Team memory sync | 🔵 | 🟡 | `TeamMemorySyncService` exists, not wired |
| Auto-dream | 🔵 | 🟡 | `AutoDream` exists, not wired |

## Agent Management

| Capability | cc-src | coderex | Notes |
|------------|--------|---------|-------|
| Agent spawning | ✅ | ✅ | `AgentManager::spawn_agent` |
| Context fork | ✅ | ✅ | `ForkedContext` |
| Tool whitelist | ✅ | ✅ | `SpawnOptions::allowed_tools` |
| Message channel | ✅ | ✅ | `mpsc::UnboundedSender` |
| Lifecycle tracking | ✅ | ✅ | `TaskManager` integration |
| Teammate events | ✅ | ✅ | `TeammateEvent` callback |
| Resume support | ✅ | ✅ | `AgentManager::resume_agent` |
| Forked agent execution | ✅ | 🟡 | In `orchestrator`, moving to `AgentManager` |

## Gaps for Production

1. **Memory workflow not connected to REPL** — `/update-memory` and `/extract-memories` spawn background tasks but do not persist to `SessionMemoryStore` / `LongTermStore`.
2. **AgentManager lacks ApiClient** — `AgentManager` cannot execute forked agents directly; it delegates to `orchestrator` functions in `coderex-core`.
3. **No prompt suggestion UI** — Background prompt suggestions are generated but not shown inline in the TUI input area.
4. **Compact summary not integrated** — `/compact` runs but does not truncate the session context.
