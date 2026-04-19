# agents Module Inspection

## Goal

Sub-agent system for spawning autonomous worker agents, dream agents (background summarization), and forked contexts. Enables parallel task execution and delegated work items.

## Design

### Key Types

- **`AgentManager`** (`manager.rs`) — Spawns, tracks, and communicates with sub-agents.
- **`AgentInstance`** (`manager.rs`) — Running agent handle with channels.
- **`AgentType`** (`types.rs`) — GeneralPurpose, Research, CodeReview, etc.
- **`Thoroughness`** (`types.rs`) — Quick, Medium, Thorough — controls agent depth.
- **`SpawnOptions`** (`manager.rs`) — Configuration for new agents.
- **`ForkedContext`** (`manager.rs`) — Snapshot of parent context for agent delegation.
- **`AgentMessage`** (`manager.rs`) — Communication protocol between parent and agents.
- **`TeammateEvent`** (`manager.rs`) — Agent lifecycle events.

### Architecture Patterns

- **Channel-based communication** — Parent and child agents communicate via mpsc.
- **Context forking** — Parent snapshots its state for the child to work from.
- **Thoroughness levels** — Agents adjust their approach based on requested depth.

## Current Status

### What Works

- Agent type and thoroughness definitions.
- Spawn options and context forking types.
- Agent manager skeleton with error types.
- `execute_via_agent()` and `spawn_dream_agent()` entry points.

### What Is Stubbed/Partial

- **Agent execution** — Types are rich but actual agent spawning may not fully connect to core engine.
- **Dream agent** — Configuration exists but background execution not wired.
- **Teammate events** — Event types defined but no event bus integration.

## Gaps for Production

- **No agent isolation**: Sub-agents share process memory; no sandboxing.
  → **Suggestion**: Spawn agents as separate OS processes using `std::process::Command` or a WASM runtime (`wasmtime`). Use IPC (Unix domain sockets or gRPC) for communication. Each agent gets its own filesystem sandbox.
- **No agent resource limits**: Spawned agents can consume unbounded tokens/time.
  → **Suggestion**: Add `max_tokens` and `max_duration_seconds` to `SpawnOptions`. Enforce via `tokio::time::timeout` and token budget subtraction from the parent. Kill agents that exceed limits.
- **No agent result validation**: Agent outputs are not verified for correctness.
  → **Suggestion**: Implement a validation pipeline where agent results are passed through a lightweight checker (e.g., syntax validation for code, JSON schema for structured output). Reject and retry up to 2 times.
- **No agent retry/fallback**: Failed agents do not automatically retry or escalate.
  → **Suggestion**: Add an `AutoRetry` policy to `SpawnOptions`: on failure, retry with `Thoroughness::Thorough` and a more detailed prompt. After max retries, escalate to the parent with a failure summary.
- **Limited agent types**: Only a few predefined types; no custom agent definitions.
  → **Suggestion**: Allow users to define custom agent types in `~/.coderex/agents/{name}.json` with fields: `system_prompt`, `allowed_tools`, `model`, `max_tokens`. Load these at startup alongside built-in types.
- **No agent observability**: Cannot inspect what an agent is doing in real-time.
  → **Suggestion**: Stream agent events (thoughts, tool calls, outputs) back to the parent via a broadcast channel. Render them in a dedicated UI panel or log to `~/.coderex/agent_logs/{agent_id}.jsonl`.
