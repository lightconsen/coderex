# tools Module Inspection

## Goal

Implements the tool trait system, registers built-in tools, and provides execution infrastructure for all operations the LLM can invoke: file I/O, shell commands, web search, task management, cron scheduling, and more.

## Design

### Key Types

- **`Tool` trait** (`tool.rs:74`) — Core abstraction: `descriptor()` returns metadata, `execute()` runs the tool. All tools implement this.
- **`ToolDescriptor`** (`tool.rs:48`) — Metadata: name, tier (Baseline/FeatureGated/DynamicMcp/Internal), input/output schemas, approval requirement, read-only flag, concurrency safety, interrupt behavior.
- **`ToolContext`** (`tool.rs:84`) — Runtime context: execution mode, cwd, shared state map.
- **`ToolInput`** (`tool.rs:37`) — call_id, name, JSON arguments.
- **`ToolRegistry`** (`registry.rs`) — DashMap-backed concurrent registry with deny rules.
- **`ToolExecutor` trait** (`executor.rs`) — Higher-level abstraction for batch execution.
- **`RegistryToolDispatcher`** (`dispatcher.rs`) — Bridges registry to executor.

### Built-in Tools (28 total)

| Tool | Category | Status |
|------|----------|--------|
| Read, Write, Edit, Glob, Grep | File I/O | Functional |
| Bash, PowerShell | Shell execution | Functional (with sandbox) |
| WebSearch, WebFetch | Web | Functional |
| Skill | Skill loading | Functional (reads .md files) |
| Brief | Session summary | Functional |
| TaskCreate, TaskGet, TaskList, TaskUpdate, TaskStop, TaskOutput | Task management | Functional (in-memory) |
| CronCreate, CronDelete, CronList | Cron scheduling | Functional (wraps `cron` crate) |
| TodoWrite | Todo tracking | Functional |
| AskUserQuestion | Interactive | Functional |
| SendMessage, EnterPlanMode, ExitPlanMode | Agent flow | Functional |
| NotebookEdit | Jupyter | Functional |
| LspTool | LSP integration | Stub |
| McpAuthTool | MCP auth | Stub |
| ConfigTool | Settings read | Functional |
| Echo, NoOp, Sleep | Utilities | Functional |
| RemoteTriggerTool | Remote execution | Stub |
| ReplTool, AgentTool | REPL/Agent | Partial |
| StructuredOutputTool, SyntheticOutputTool | Output formatting | Partial |

### Architecture Patterns

- **Trait-based plugin system** — All tools implement `Tool` with `async_trait`.
- **Tier-based gating** — Tools classified by availability level.
- **Concurrent registry** — `DashMap` for thread-safe tool lookup.
- **Shared state via `Arc<HashMap<String, SharedState>>`** — Tools can access task stores, cron schedulers, etc.

## Current Status

### What Works

- 28 built-in tools registered and functional (most file/shell/web/task tools).
- Tool registry with deny rules and concurrent access.
- `ToolExecutor` batch execution with permission integration.
- Task store and todo store as shared state.
- Cron scheduler wrapper (parses expressions, computes next triggers).
- All major tools have unit tests.

### What Is Stubbed/Partial

- **LspTool** — Exists but minimal implementation.
- **McpAuthTool** — Auth flow not fully implemented.
- **RemoteTriggerTool** — No actual remote execution.
- **Cron jobs never execute** — `CronScheduler` parses but no runner triggers jobs.
- **Task store is in-memory only** — No persistence across restarts.

## Gaps for Production

- **SECURITY — Shell sandbox is basic**: `SandboxConfig` exists but sandboxing relies on OS-level features; command injection still possible via crafted arguments.
  → **Suggestion**: Implement a proper sandbox using `seccomp-bpf` (Linux) or `sandbox-exec` (macOS) for BashTool. Parse and validate command arguments against an allowlist before execution.
- **No tool output size limits**: Large file reads or command outputs could OOM the process.
  → **Suggestion**: Add a `max_output_bytes` field to `ToolContext` (default 1MB). Truncate outputs with a "... [truncated]" suffix and emit a warning event.
- **No tool timeout enforcement**: `BashTool` can hang indefinitely.
  → **Suggestion**: Wrap all tool `execute()` calls in `tokio::time::timeout` with a configurable duration (default 60s for shell, 10s for file ops). Return a timeout-specific error variant.
- **No tool result caching**: Repeated identical tool calls hit the filesystem every time.
  → **Suggestion**: Add an LRU cache keyed by `(tool_name, arguments_hash)` in `ToolRegistry`. Invalidate on file modification time for file-read tools. Use `moka` crate for concurrent caching.
- **WebFetch may follow redirects to arbitrary domains**: No domain whitelist.
  → **Suggestion**: Add `allowed_domains` to `WebFetch` tool config. Use `reqwest` redirect policy to intercept and validate the final URL against the whitelist before following.
- **No rate limiting on WebSearch/WebFetch**: Could hit API limits or be used for abuse.
  → **Suggestion**: Implement a per-tool token bucket rate limiter (e.g., `governor` crate). Default: 10 requests/minute for WebSearch, 30/minute for WebFetch. Track per-session.
- **Task store not persisted**: All tasks lost on restart.
  → **Suggestion**: Persist tasks to a JSONL file in `~/.coderex/tasks.jsonl`. Load on startup, append on create/update, rewrite on delete. Use atomic rename for writes.
- **Cron scheduler has no runner**: Jobs are scheduled but never fired.
  → **Suggestion**: Spawn a background tokio task that sleeps until the next cron trigger, then executes the associated tool call. Store job state in SQLite for durability across restarts.
