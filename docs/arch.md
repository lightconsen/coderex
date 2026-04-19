# CodeRex Architecture

## 1. Workspace Layout (15 crates)

| Crate | Responsibility |
|-------|--------------|
| `cli` | Binary + startup orchestrator (StartupOrchestrator -> REPL/server) |
| `core` | Types, QueryEngine, TurnState machine, permission engine, API client |
| `tools` | Tool trait, registry, executor, 30+ built-in tools (Read/Write/Bash/etc.) |
| `commands` | Slash command registry, 50+ commands (/commit, /compact, /bridge, etc.) |
| `ui` | ratatui terminal UI, statusline, vim mode, overlays, themes |
| `memory` | JSONL transcript store, SQLite session index, long-term memory |
| `agents` | AgentManager, AgentType enum, spawn/fork/SendMessage |
| `mcp` | MCP server lifecycle, tool/resource discovery, OAuth, elicitation |
| `plugins` | PluginManager, marketplace reconciliation, manifest loading |
| `bridge` | Remote session orchestration, transport, heartbeat, state machine |
| `server` | WebSocket daemon server, session auth, idle cleanup |
| `hooks` | Hook event system (22 events), sync/async execution, timeout |
| `config` | Settings merge (defaults->user->project), feature flags, enterprise |
| `sdk` | Structured I/O event stream for programmatic consumers |

Commands (user-facing slash commands) are separate from Tools (LLM-facing function calls).

## 2. Data Flow: A Single User Turn

```
[User Input]
     |
     v
+-----------+    +-----------+    +-----------+
|  REPL/UI  |--->| QueryEngine|--->| AnthropicApi|
|  (cli/ui) |    |   (core)  |    |  Client    |
+-----------+    +-----------+    +------+-----+
     ^                 |                   |
     |           +-----+-----+             |
     |           v           v             |
     |   [TurnState machine]        [SSE Stream]
     |   Initialize->BuildContext->  TextDelta / ToolCallStart
     |   StartRequest->StreamAssistant-->ToolCallEnd
     |           |                          |
     |           |    +---------------------+
     |           |    v
     |           |  Tool calls ---> ToolExecutor (tools/)
     |           |    |                   |
     |           |    |    +--------------+
     |           |    |    v
     |           |    |  Read/Write/Bash/Glob/Grep/WebFetch/etc.
     |           |    |    |
     |           |    |    v (results)
     |           |    |  ApplyToolResults -> EvaluateContinuation
     |           |    |    |
     |           +----+----+
     |                 |
     |                 v
     |          Finalize -> [Output to user]
     |                 |
     +-----------------+
```

## 3. Startup Orchestrator (12 phases)

```
Bootstrap -> LoadSettings -> BuildCapabilities -> LoadCommands
 |              |                                  (scan .claude/commands/)
 |              +-- RuntimeOverrides (CLI flags, env vars)
 |
 +-- tracing init, signal handlers

LoadSkills -> InitPlugins -> ReconcileMarketplaces -> InitMcp
   |             |                |                      |
   |             |                |                      +-- .mcp.json -> McpManager.start_all()
   |             |                +-- MarketplaceReconciler.reconcile()
   |             +-- PluginManager.discover() + load()
   +-- scan .claude/skills/

InitMemory -> InitRemoteServices -> Ready
    |             |
    |             +-- Bridge transport (if CODEREX_BRIDGE_URL set)
    +-- .claude/ directory setup
```

## 4. Key Abstractions

### QueryEngine (core)

- `submit(prompt, SubmitOptions)` -> EngineEvent stream
- TurnState machine (14 states, transition validation, dead-loop detection)
- PromptCompiler (static + dynamic sections, boundary markers)
- TokenBudget (threshold tracking, continuation limits)
- CostTracker (per-turn/session/agent/task cost accumulation)

### Tool System (tools)

- Tool trait: `descriptor()` + `execute(input, ToolContext)`
- ToolRegistry: Baseline + FeatureGated + DynamicMcp tiers
- ToolExecutor: parallel (concurrency_safe) + serial scheduling
- ExecutionContext: Normal | AsyncAgent | CoordinatorMode | PlanMode | ...

### Permission Engine (core)

- PermissionMode: Default | AcceptEdits | BypassPermissions | Plan | Auto
- PermissionChecker: allow/deny/ask rule merging, path allowlists
- Hook short-circuit: hooks can override permission decisions
- Sandbox: seccomp (Linux) | sandbox-exec (macOS) | Job Objects (Windows)

### Session Storage (memory)

- TranscriptStore: JSONL append, read_all, skip-first-prompt
- SessionIndex: SQLite (sessions.db), create/update/list/recent
- LongTermStore: ~/.claude/memory/ file-based, fuzzy search

## 5. Notable Patterns

### Tiered Tool Registry

- **Baseline** — always on (Read, Write, Bash, Glob, Grep, etc.)
- **FeatureGated** — flag-controlled (WebBrowser, TerminalCapture, CtxInspect, etc.)
- **DynamicMcp** — runtime-discovered from MCP servers

Commands have an availability matrix (non-interactive / remote-safe / bridge-safe).

### Startup Barrier Pattern

Remote settings and MCP use loading barriers so other systems can await initialization without blocking the full chain. Fail-open on timeout.

### Agent as Subprocess

Agent spawn creates a new TaskHandle with forked context and a custom tool whitelist. SendMessage delivers messages across agent boundaries.

### Compact System

Four strategies: Micro (trim tool results), Auto (token threshold), Reactive (context overflow), Snip (history truncation). All preserve boundary markers in transcripts.

### Bridge State Machine

Assigned -> Preparing -> Spawning -> Running -> Archiving -> CleaningUp -> Completed

Heartbeat timeout triggers cleanup. Worktree isolation per session.

## 6. Test Pyramid

### 500+ unit tests across all crates

- Types serde roundtrip, Tool execution, Permission decisions
- TurnState transitions, PromptCompiler rendering, Cost calculations
- Command handlers, Registry lookups, File I/O tools

### Integration tests

- Transcript write/read, SessionIndex CRUD, Plugin discovery
- MCP server spawn/stop, Bridge transport send/receive
- QueryEngine mock-client end-to-end

### E2E tests (cli)

- MockApiClient simulates Anthropic responses
- Full REPL loop: input -> engine -> stream -> tool -> result
- Stress: 5000 messages, 1000 commands, 10000-char input
