# tools Module Inspection

## Goal

Implements the tool trait system, registers built-in tools, and provides execution infrastructure for all operations the LLM can invoke: file I/O, shell commands, web search, task management, cron scheduling, MCP auth, and more.

## Design

### Key Types

- **`Tool` trait** (`tool.rs:74`) — Core abstraction: `descriptor()` returns metadata, `execute()` runs the tool. All tools implement this via `async_trait`.
- **`ToolDescriptor`** (`tool.rs:48`) — Metadata: name, tier (Baseline/FeatureGated/DynamicMcp/Internal), input/output JSON schemas, approval requirement, read-only flag, concurrency safety, interrupt behavior.
- **`ToolContext`** (`tool.rs:84`) — Runtime context: `execution_context` (Normal, AsyncAgent, CoordinatorMode, InProcessTeam, PlanMode, ReplOnly, HeadlessSdk, RemoteBridge), `cwd`, and `extra` shared state map (`Arc<HashMap<String, SharedState>>`).
- **`ToolInput`** (`tool.rs:37`) — `call_id`, `name`, JSON `arguments`.
- **`ToolOutput`** (`context.rs:66`) — `output: String` and `is_display: bool`.
- **`ToolError`** (`context.rs:28`) — `NotFound`, `Timeout`, `Io`, `InvalidInput`, `PermissionDenied`, `RateLimited`.
- **`ToolRegistry`** (`registry.rs`) — `DashMap`-backed concurrent registry with deny rules and tier filtering.
- **`ToolExecutor`** (`executor.rs`) — Concrete executor with concurrency control, per-tool serialization locks, timeout enforcement, and output truncation.
- **`RegistryToolDispatcher`** (`dispatcher.rs`) — Bridges `coderex_core::tool_executor::ToolExecutor` trait to the in-crate registry + executor. Injects shared state (sandbox config, task store, cron scheduler, rate limiter) into `ToolContext.extra`.
- **`RateLimiter`** (`rate_limiter.rs`) — Per-tool token-bucket rate limiter tracking request timestamps in a `DashMap<String, Vec<Instant>>`.
- **`TaskStore`** (`task_store.rs`) — `DashMap`-backed task registry with JSONL persistence (`~/.coderex/tasks.jsonl`). Supports create, update, delete, list, filter by status.
- **`CronScheduler`** (`cron_scheduler.rs`) — In-memory cron job scheduler with optional file persistence for durable jobs. Includes `spawn_runner()` which returns an `mpsc::UnboundedReceiver<(job_id, prompt)>` fired by a background tokio task polling every 60s.
- **`SandboxConfig`** (`sandbox_policy.rs`) — Default per-tool sandbox policies injected by `RegistryToolDispatcher` when sandboxing is enabled.

### Built-in Tools (44 baseline + 13 feature-gated)

#### Baseline Tools

| Tool | Category | Status |
|------|----------|--------|
| Read, Write, Edit, Glob, Grep, NotebookEdit | File I/O | Functional |
| Bash, PowerShell | Shell execution | Functional (sandbox + allowlist + metacharacter rejection) |
| WebSearch, WebFetch | Web | Functional (rate limiting, domain whitelist, redirect validation) |
| Skill | Skill loading | Functional (reads `.md` files) |
| Brief | Session summary | Functional |
| TaskCreate, TaskGet, TaskList, TaskUpdate, TaskStop, TaskOutput | Task management | Functional (JSONL persistence via `TaskStore`) |
| CronCreate, CronDelete, CronList | Cron scheduling | Functional (background runner via `CronScheduler::spawn_runner`) |
| TodoWrite | Todo tracking | Functional |
| AskUserQuestion | Interactive | Functional |
| SendMessage, EnterPlanMode, ExitPlanMode | Agent flow | Functional |
| LspTool | LSP integration | Functional (spawns language server, JSON-RPC stdio) |
| McpAuthTool | MCP auth | Functional (OAuth flow: browser open, localhost callback, token exchange, persistence to `~/.coderex/mcp_tokens.json`) |
| ListMcpResourcesTool, ReadMcpResourceTool | MCP resources | Functional |
| TeamCreateTool, TeamDeleteTool | Team management | Functional |
| EnterWorktreeTool, ExitWorktreeTool | Worktree management | Functional |
| ConfigTool | Settings read | Functional |
| Echo, NoOp, Sleep | Utilities | Functional |
| RemoteTriggerTool | Remote execution | Functional (HTTP POST to bridge server) |
| ReplTool | REPL command | Functional (intentional no-op — handled by session UI) |
| AgentTool | Agent spawning | Functional (integrates with AgentManager) |
| StructuredOutputTool | Output validation | Functional (custom JSON schema validator) |
| SyntheticOutputTool | Output formatting | Functional (plain/json/display modes) |
| ToolSearchTool | Tool discovery | Functional |

#### Feature-Gated Tools (13)

Registered with `ToolTier::FeatureGated` and `is_enabled: false`. Return a "feature-gated and not enabled" message when invoked.

| Tool | Description |
|------|-------------|
| WebBrowser | Browser automation |
| TerminalCapture | Terminal output capture |
| CtxInspect | Context inspection |
| Monitor | System monitoring |
| WorkflowTool | Workflow execution |
| SnipTool | Code snippet management |
| VerifyPlanExecution | Plan verification |
| SuggestBackgroundPR | Background PR suggestions |
| PushNotification | Push notifications |
| SendUserFile | File sharing |
| SubscribePR | PR subscription |
| ListPeers | Peer discovery |
| Tungsten | Tungsten integration |

### Architecture Patterns

- **Trait-based plugin system** — All tools implement `Tool` with `async_trait`. The trait is object-safe and tools are stored as `Arc<dyn Tool>`.
- **Tier-based gating** — Tools classified by `ToolTier`: `Baseline` (always available), `FeatureGated` (disabled by default), `DynamicMcp` (discovered at runtime), `Internal` (debugging).
- **Concurrent registry** — `DashMap<String, Arc<dyn Tool>>` for thread-safe tool lookup. Supports deny rules (`deny_list`) and tier filtering.
- **Shared state injection** — `RegistryToolDispatcher` injects shared services into `ToolContext.extra` before execution:
  - `"sandbox_config"` → `SandboxConfig` (per-tool policy from `sandbox_policy.rs`)
  - `"task_store"` → `Arc<TaskStore>` (for task management tools)
  - `"cron_scheduler"` → `Arc<CronScheduler>` (for cron tools)
  - `"rate_limiter"` → `Arc<RateLimiter>` (for web tools)
  - `"web_fetch_cache"` → `Arc<WebFetchCache>` (for `WebFetchTool` URL cache)
- **Concurrency serialization** — `ToolExecutor` maintains per-tool-name `Arc<Mutex<()>>` locks. Tools marked `is_concurrency_safe: false` are serialized; safe tools run in parallel up to `max_concurrent`.
- **Output truncation** — `ExecutionConfig.max_output_bytes` (default 1MB) enforced by `ToolExecutor`. Outputs beyond the limit are truncated with `"\n... [truncated]"`.
- **Timeout enforcement** — All tool executions wrapped in `tokio::time::timeout` using `ExecutionConfig.default_timeout` (default 60s).
- **Batch execution** — `execute_many()` chunks calls into batches of `max_concurrent` and runs each batch concurrently.

## Current Status

### What Works

- **44 baseline tools** registered and functional.
- **13 feature-gated tools** registered (disabled by default).
- Tool registry with deny rules, tier filtering, and concurrent access.
- `ToolExecutor` batch execution with permission integration, output truncation, timeout, and concurrency control.
- `RegistryToolDispatcher` bridges the core `ToolExecutor` trait to the concrete tool pipeline with full shared state injection.
- `TaskStore` with JSONL persistence (`~/.coderex/tasks.jsonl`). Load on startup, append on create/update, rewrite on delete.
- `CronScheduler` with background tokio runner (60s poll interval, `MissedTickBehavior::Skip`). One-shot jobs auto-delete after firing; recurring jobs auto-expire after 7 days.
- `RateLimiter` with per-tool token-bucket tracking. `WebSearchTool` enforces 10 req/min; `WebFetchTool` enforces 30 req/min.
- `WebFetchTool` redirect validation via `reqwest::redirect::Policy::custom` against `allowed_domains` whitelist. Simple in-memory URL cache with 15-minute TTL.
- `BashTool` command allowlist + shell metacharacter rejection (`;`, `|`, `&`, `` ` ``, `$`, `(`, `)`, `<`, `>`) + OS-level sandbox integration (macOS `sandbox-exec`, Linux `seccomp-bpf`).
- `McpAuthTool` full OAuth flow: `open_browser()` → `wait_for_callback()` on `localhost:8765` → code exchange POST → `persist_token()` to `~/.coderex/mcp_tokens.json`.
- All major tools have unit tests (167+ tests in `coderex-tools`).

### What Is Stubbed/Partial

None — all previously stubbed items are now implemented.

## Gaps for Production

- **No global tool result caching** — `WebFetchTool` has a simple per-URL cache (15-min TTL), but repeated identical calls to file, shell, or search tools hit the underlying system every time.
  - **Suggestion**: Add an LRU cache keyed by `(tool_name, arguments_hash)` in `ToolRegistry`. Invalidate on file modification time for file-read tools. Use `moka` crate for concurrent caching.
