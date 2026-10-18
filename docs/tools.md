# tools Module Inspection

## Goal

Implements the tool trait system, registers built-in tools, and provides execution infrastructure for all operations the LLM can invoke: file I/O, shell commands, web search, task management, cron scheduling, MCP auth, and more.

## Design

### Key Types

- **`Tool` trait** (`tool.rs`) — Core abstraction: `descriptor()` returns metadata, `capabilities()` returns what the tool declares about itself, `execute()` runs the tool. All tools implement this via `async_trait`.
- **`ToolDescriptor`** (`tool.rs`) — Metadata: name, tier (Baseline/FeatureGated/DynamicMcp), input/output JSON schemas, approval requirement, read-only flag, concurrency safety, interrupt behavior.
- **`ToolCapabilities`** (`tool.rs`) — What a tool declares that has no home in `ToolDescriptor`: `max_result_size_chars`, `rule_content_field`, `backgrounding`, `ignores_whole_tool_allow_rule`, `suppresses_always_allow_rule`. Reached through the defaulted `Tool::capabilities()` method rather than four more `ToolDescriptor` fields, because `ToolDescriptor` is built with a struct literal at 100+ sites across the workspace, several in crates `coderex-tools` does not own. Every default is the pre-existing behaviour, so a tool that declares nothing is unchanged.
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
- **`ToolCache`** (`tool_cache.rs`) — Global LRU cache for read-only tool results using `moka`. Keys are `(tool_name, arguments_hash)`. `Read` tool entries are invalidated when the file's mtime changes.

### Built-in Tools (45 baseline + 13 feature-gated)

#### Baseline Tools

| Tool | Category | Status |
|------|----------|--------|
| Read, Write, Edit, Glob, Grep, NotebookEdit | File I/O | Functional |
| Bash, PowerShell | Shell execution | Functional (sandbox + allowlist + metacharacter rejection). **One of the two is registered, chosen by platform**: PowerShell on Windows, Bash everywhere else — see below. |
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

#### Feature-Gated Tools

The only `FeatureGated` tools are the five Chrome DevTools tools, which are
enabled (`is_enabled: true`) and controlled by the session's Chrome capability
(`--no-chrome`). Thirteen placeholder tools (WebBrowser, TerminalCapture,
CtxInspect, Monitor, WorkflowTool, SnipTool, VerifyPlanExecution,
SuggestBackgroundPR, PushNotification, SendUserFile, SubscribePR, ListPeers,
Tungsten) were removed: they were permanently `is_enabled: false`, no feature
flag could turn them on, the dispatcher filtered them out of every listing, and
their only behavior was a refusal string. `register_builtin_tools` now registers
45 baseline tools plus 5 Chrome tools.

### The result-size ladder

Seven tiers, mirroring the reference's `maxResultSizeChars`. A tool declares one; the executor no longer applies one size to every tool alike. `1`/`0` is the reference's spelling of "no cap" and maps to `Unlimited`, which **only `Read` declares** — and only because Read bounds its own output (`MAX_DISPLAY_CHARS`) before returning it.

| Tier | Characters | Declared by |
|------|-----------|-------------|
| `Unlimited` (`1`/`0`) | — | `Read` |
| `Tiny` | 10 000 | `TaskOutput` (matches its own `MAX_OUTPUT_CHARS`) |
| `Small` | 20 000 | — |
| `Medium` | 50 000 | — |
| `Large` | 100 000 | — |
| `XLarge` | 300 000 | — |
| `Huge` | 1 048 576 | the default — every tool that declares nothing, i.e. all of them but the two above |

`Bash` and `PowerShell` cut themselves at 50 000 bytes (`MAX_OUTPUT_BYTES`) and so belong in `Medium`, but those two files are owned elsewhere; until their owner declares it they keep the `Huge` default and their internal cut, which is the pre-existing behaviour either way.

### Architecture Patterns

- **Trait-based plugin system** — All tools implement `Tool` with `async_trait`. The trait is object-safe and tools are stored as `Arc<dyn Tool>`.
- **Tier-based gating** — Tools classified by `ToolTier`: `Baseline` (always available), `FeatureGated` (disabled by default), `DynamicMcp` (discovered at runtime).
- **Concurrent registry** — `DashMap<String, Arc<dyn Tool>>` for thread-safe tool lookup. Supports deny rules (`deny_list`) and tier filtering.
- **Shared state injection** — `RegistryToolDispatcher` injects shared services into `ToolContext.extra` before execution:
  - `"sandbox_config"` → `SandboxConfig` (per-tool policy from `sandbox_policy.rs`)
  - `"task_store"` → `Arc<TaskStore>` (for task management tools)
  - `"cron_scheduler"` → `Arc<CronScheduler>` (for cron tools)
  - `"rate_limiter"` → `Arc<RateLimiter>` (for web tools)
  - `"web_fetch_cache"` → `Arc<WebFetchCache>` (for `WebFetchTool` URL cache)
  - `"job_session_id"` → `String` (the owning session, for tools that write durable job records; set by `RegistryToolDispatcher::set_session_id`)
- **Concurrency serialization** — `ToolExecutor` maintains per-tool-name `Arc<Mutex<()>>` locks. Tools marked `is_concurrency_safe: false` are serialized; safe tools run in parallel up to `max_concurrent`.
- **Output truncation** — each tool's own `MaxResultSizeChars` tier is the cap, applied by `ToolExecutor`; `ExecutionConfig.max_result_size_chars` (default 1 048 576) is an operator *ceiling* that can only lower it. Outputs beyond the limit are truncated with `"\n... [truncated]"`, counted in **characters** so the cut always lands on a character boundary.
- **Rule content fields** — a deny rule is judged against the value the tool names in `rule_content_field` (`Read` declares `file_path`); a tool that declares nothing falls back to the known path fields, then to anything path-shaped, because for a deny rule the safe direction to be wrong in is "matched".
- **Backgrounding** — `backgrounding` is `None` / `SelfManaged` / `Detach`. A tool that does not declare one has `run_in_background` removed from the schema the model is shown, rather than advertising a flag it ignores. `Bash` declares `SelfManaged`: with `run_in_background: true` it spawns the child, writes a `JobKind::Shell` record under `<cwd>/.coderex/jobs`, returns `{job_id, pid, output_file}` immediately, and a tokio task settles the record `Done`/`Failed` (with exit code or timeout error) when the child ends. Combined output is captured at `<cwd>/.coderex/jobs/<job_id>.out`. `Agent` uses the same durable record for `run_in_background` sub-agents (`JobKind::Agent`), settling `Done`/`Failed` on completion and `Interrupted` on stop or manager drop. See `docs/agents.md` for the adoption side.
- **Rule exemptions** — declared per *call*, not per tool. `Tool::rule_exemptions_for(&self, input)` defaults to the tool's static `capabilities().rule_exemptions()`; `ToolRegistry::rule_exemptions_for(tool_name, input)` answers for one call (`ToolRuleExemptions::NONE` for a name it does not know). `ignores_whole_tool_allow_rule` is consulted **only when a whole-tool allow rule matched**: the rule is skipped and the next matching rule — or the ordinary mode default below — decides. It does **not** mean "never auto-approved": a call that carries the flag and finds no such rule gets exactly the default every other tool gets (`is_dangerous_tool` → Ask, else Allow). `suppresses_always_allow_rule` withholds the session-wide choice from the confirmation prompt (`allow_session: false`) **and** refuses to write one: an `ApproveSession` answer still approves that call, but is not recorded in the checker's session-approval cache, so the next call asks again. Both flags narrow *allow* rules only — a deny rule is evaluated first and still wins.
  - **Carriers:** the **Artifact** family declares both flags per input action — mutating actions only (`publish`/`delete`/`upload_asset`/`delete_asset`, `set`/`update`/`delete`/`str_replace`/`batch`, `add`/`reply`/`resolve`); reads and `verify` carry neither. `Agent` and `Workflow` declare neither: delegation is not one of the reference's carriers, and a sub-agent's own tool calls are gated by that same checker anyway.
  - **Install:** `install_rule_exemptions(&mut PermissionEngine, &Arc<ToolRegistry>)` installs a resolver closure so the engine asks the registry per call (without one it falls back to the checker's static table). The CLI's call, which needs a `QueryEngine::permission_engine_mut()` accessor and must run **before** `RegistryToolDispatcher::new` consumes the `Arc<ToolRegistry>`:
    ```rust
    coderex_tools::install_rule_exemptions(engine.permission_engine_mut(), &registry);
    ```
- **Timeout enforcement** — All tool executions wrapped in `tokio::time::timeout` using `ExecutionConfig.default_timeout` (default 60s).
- **Batch execution** — `execute_many()` chunks calls into batches of `max_concurrent` and runs each batch concurrently.
- **Result caching** — `ToolExecutor` caches successful results from read-only tools in an LRU cache (`moka`, default 1000 entries, 5-minute TTL). `Read` tool cache entries are invalidated when the underlying file's mtime changes.

## The platform's shell

There is one shell role with two platform spellings, and the platform picks
which: `platform_shell_is_powershell()` is `cfg!(windows)`, and
`register_builtin_tools` registers that one. The reference does the same — its
REPL `!` mode calls the Bash tool and "PowerShell 平台走 PowerShellTool", its
headless path uses `/bin/sh -c` and "Windows 用 `pwsh -NoProfile -Command`"
(`subsystems/bash.md` §3.12), its sandbox layer wraps "the Bash tool / the
PowerShell tool", and its permission classes are `["Bash","PowerShell"]`.

Registering both, as this did, advertised a shell the platform does not use: on
a machine with no `pwsh` the model could call `PowerShell` and get a spawn
failure — and it reached a tool with no sandbox path wired, because the sandbox
wrapper was attached to Bash only. The non-native tool still exists and still
compiles (`the_unused_shell_tool_still_exists`); it is simply not offered where
it is not the shell.

`sandbox_policy::default_sandbox_config` keeps its entry for both names, because
on Windows the `PowerShell` entry is the live one.

## Current Status

### What Works

- **45 baseline tools** registered and functional.
- **13 feature-gated tools** registered (disabled by default).
- Tool registry with deny rules, tier filtering, and concurrent access.
- `ToolExecutor` batch execution with permission integration, output truncation, timeout, and concurrency control.
- `RegistryToolDispatcher` bridges the core `ToolExecutor` trait to the concrete tool pipeline with full shared state injection.
- `TaskStore` with JSONL persistence (`~/.coderex/tasks.jsonl`). Load on startup, append on create/update, rewrite on delete.
- `CronScheduler` with background tokio runner (60s poll interval, `MissedTickBehavior::Skip`). One-shot jobs auto-delete after firing; recurring jobs auto-expire after 7 days. When constructed with `with_job_records(cwd, session_id)`, durable definitions are mirrored as `JobKind::Cron` records (`load` refreshes them to `Running`; `create` writes them; a recurring fire touches; a one-shot fire or `delete` settles `Done`). The session's cron consumer must call `mark_fired(job_id)` after handling a fired prompt — the runner only yields, so without it one-shot definitions are never retired.
- `RateLimiter` with per-tool token-bucket tracking. `WebSearchTool` enforces 10 req/min; `WebFetchTool` enforces 30 req/min.
- `WebFetchTool` redirect validation via `reqwest::redirect::Policy::custom` against `allowed_domains` whitelist. Simple in-memory URL cache with 15-minute TTL.
- `BashTool` command allowlist + shell metacharacter rejection (`;`, `|`, `&`, `` ` ``, `$`, `(`, `)`, `<`, `>`) + OS-level sandbox integration (macOS `sandbox-exec`, Linux `seccomp-bpf`) + real background execution (`run_in_background`) with durable job records.
- `McpAuthTool` full OAuth flow: `open_browser()` → `wait_for_callback()` on `localhost:8765` → code exchange POST → `persist_token()` to `~/.coderex/mcp_tokens.json`.
- `ToolCache` global LRU cache for read-only tool results. Keys are `(tool_name, arguments_hash)` with 5-minute TTL and 1000-entry capacity. `Read` tool entries invalidated on file mtime change.
- All major tools have unit tests (170+ tests in `coderex-tools`).

### What Is Stubbed/Partial

None. The thirteen `FeatureGated` placeholders that only returned a refusal were
removed rather than kept as advertised-but-unusable tools.

## Gaps for Production

None for the built-in tool set. The deliberate differences are recorded below
(rule-exemption vocabulary) and in the subsystem docs.

### Rule-exemption gaps (recorded, not implemented)

The reference's exemption vocabulary has four members CodeRex does not carry. Each is deferred deliberately; none of them is wired to a tool today.

- **`excludedFromAutoModeAllowlist`** — a tool that `Auto` mode must not auto-approve even though it is not dangerous. CodeRex's `Auto` mode has no per-tool allowlist to exclude *from*: it routes through the classifier hook the CLI registers ahead of the checker, not through a tool list, so there is nothing to hang the flag on.
- **`excludedFromModeBypassAllow`** — a tool that even a bypassing mode (`mode.bypasses_all()`) must not wave through. Only the reference's browser tool sets it. CodeRex's mode bypass is a single `mode.bypasses_all()` check at the top of `check`, before any rule or exemption — teaching it to honour a per-tool veto means threading the resolver into the mode shortcut as well.
- **`suppressesAllPermissionUpdates`** — the stronger sibling of `suppresses_always_allow_rule`: not just the session-wide allow, but *any* permission update (persisted rules included). CodeRex has no permission-update channel beyond the in-memory session-approval cache, which `suppresses_always_allow_rule` already covers; the stronger flag would be a no-op until there is one.
- **The MCP-composed conditional** — a tool composed from an MCP server suppresses the session-wide allow only while the session is interactive. CodeRex's MCP tools are `DynamicMcp` registry entries and are not composed through a wrapper that knows whether the session is interactive, so the condition has no home yet.
