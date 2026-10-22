# CodeRex AI Architecture

## Overview

CodeRex is a Rust-native AI coding agent system built around a 4-layer architecture. This document describes the AI design and implementation across the `crates/` workspace.

## 1. Four-Layer Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Feature Layer                                              │
│  - Background features (memory, docs, suggestions)          │
│  - Command-driven prompt profiles                           │
│  - Chrome MCP bridge, teleport, session sharing             │
├─────────────────────────────────────────────────────────────┤
│  AI Orchestration Layer                                     │
│  - Orchestrator: intent-aware call routing                  │
│  - QueryRequest / QueryKind / ModelProfile                  │
│  - PromptProfile registry                                   │
├─────────────────────────────────────────────────────────────┤
│  Core Execution Layer                                       │
│  - QueryEngine: turn state machine, tool loop, streaming    │
│  - PromptCompiler: static/dynamic block assembly            │
│  - PermissionEngine: hooks, classification, explainer       │
├─────────────────────────────────────────────────────────────┤
│  API Transport Layer                                        │
│  - ApiClient trait: streaming + non-streaming               │
│  - Multi-provider adapter/profile integration               │
│  - Token budget + cost tracking                             │
└─────────────────────────────────────────────────────────────┘
```

### Crate Mapping

| Layer | Crates |
|-------|--------|
| API Transport | `coderex-core` (`api_client.rs`, `types.rs`) |
| Core Execution | `coderex-core` (`engine.rs`, `prompts/`, `permissions/`) |
| AI Orchestration | `coderex-core` (`orchestrator.rs`, `query_profiles.rs`) |
| Feature | `coderex-memory`, `coderex-tools`, `coderex-commands`, `coderex-cli` |

---

## 2. Prompt System

### 2.1 PromptCompiler (`crates/core/src/prompts/base.rs`)

The `PromptCompiler` assembles the system prompt for each turn from **static** and **dynamic** blocks separated by a `BOUNDARY_MARKER`.

```rust
pub struct PromptCompiler {
    static_blocks: Vec<PromptBlock>,   // Cacheable across turns
    dynamic_blocks: Vec<(String, Box<dyn Fn(&PromptContext) -> String>)>,
    config: CompactConfig,
}
```

**Why the boundary marker matters:** Anthropic's API caches the prefix of the prompt. By placing static blocks before `BOUNDARY_MARKER` and dynamic blocks after it, the static portion hits the prompt cache on subsequent turns, reducing latency and cost.

**Assembly flow:**

1. Register static blocks (base settings, tool guidelines, output style)
2. Register dynamic renderers (session guidance, memory, denied tools, git status)
3. `assemble()` produces `AssembledContext { system_prompt, context_messages, estimated_tokens }`
4. `CLAUDE.md` from cwd/parent dirs is loaded and wrapped as a `<system-reminder>` user message
5. Git branch + status are injected as system context

### 2.2 Feature Prompts (`crates/core/src/prompts/features/`)

Nine dedicated prompt modules for single-purpose AI tasks. Each module exposes async functions that construct a `QueryRequest` and call the orchestrator.

| Module | Purpose | Call Pattern |
|--------|---------|--------------|
| `session_title` | Generate 3-7 word session title | `run_lightweight_query` |
| `permission_explainer` | Explain tool call risks in 1-2 sentences | `run_side_query` |
| `compact_summary` | Summarize conversation for context compression | `run_forked_agent` |
| `session_memory` | Update session memory file | `run_forked_agent` |
| `memory_extraction` | Extract long-term memories | `run_forked_agent` |
| `prompt_suggestion` | Suggest next user prompts | `run_background_feature` |
| `agent_summary` | Summarize sub-agent progress (3-5 words) | `run_forked_agent` |
| `agent_generation` | Generate agent config (identifier, whenToUse, systemPrompt) | `run_side_query` |
| `magic_docs` | Generate documentation / README / codebase overview | `run_forked_agent` |

All feature prompts are decoupled from the main system prompt. They define their own system/user prompt pairs and do not reuse the default agent prompt, avoiding behavior boundary coupling.

### 2.3 Prompt Profiles (`crates/core/src/query_profiles.rs` + `prompts/mod.rs`)

`PromptProfile` selects which system prompt persona to use. Profiles compose together:

```rust
pub enum PromptProfile {
    DefaultAgent,      // General coding agent
    Advisor,           // Guidance-focused, less implementation
    BugHunter,         // Systematic defect review
    Reviewer,          // Code review and analysis
    SecurityReviewer,  // Security-focused diff review
    MemoryUpdater,     // Session memory maintenance
    TitleGenerator,    // Session title generation
    PermissionExplainer, // Tool call risk explanation
}
```

**Composition rules:**
- Some profiles include the default agent prompt as a base (`include_default_agent: true`)
- Others are standalone (`include_default_agent: false`)
- Multiple profiles can be composed: `compose_system_prompt_for_profiles(&[Advisor, Reviewer])`
- Each profile may define a default allowed tool set (e.g., `BugHunter` → `["Read", "Grep"]`)

Commands activate profiles via `CommandResult::prompt_profile`:

```rust
// /advisor command
CommandResult::success("Advisor mode activated.")
    .with_prompt_profile(PromptProfile::Advisor)
```

---

## 3. Orchestrator — Call Pattern Abstraction

The `Orchestrator` (`crates/core/src/orchestrator.rs`) routes AI calls by **intent** rather than raw parameters. It provides five call primitives:

### 3.1 Call Primitives

| Function | Purpose | Model | Streaming | Tools |
|----------|---------|-------|-----------|-------|
| `run_main_agent` | Full interactive session loop | Main | Yes | Full set |
| `run_side_query` | Single-shot decision/explanation | Main | No | None |
| `run_lightweight_query` | Fast small-model tasks | Lightweight | No | None |
| `run_forked_agent` | Isolated sub-agent with tool whitelist | Configurable | No | Restricted |
| `run_background_feature` | Background tasks (memory, docs, etc.) | Background | No | Optional |

### 3.2 QueryRequest

```rust
pub struct QueryRequest {
    pub kind: QueryKind,                    // MainAgent | SideQuery | Lightweight | ForkedAgent
    pub user_prompt: String,
    pub custom_system_prompt: Option<String>,   // Full override
    pub append_system_prompt: Option<String>,   // Append to base
    pub prompt_profile: PromptProfile,
    pub allowed_tools: Option<Vec<String>>,
    pub model_profile: ModelProfile,        // Main | Lightweight | Background
    pub inherit_session: bool,
    pub stream: bool,
    pub max_tokens: u32,
}
```

**Model resolution:**
- `ModelProfile::Main` → `claude-sonnet-4-6`
- `ModelProfile::Lightweight` → `claude-haiku-4-5`
- `ModelProfile::Background` → `claude-sonnet-4-6`

### 3.3 Forked Agent Isolation

Forked agents run in an isolated context window with configurable constraints:

- **Tool whitelist:** Only allowed tools are exposed
- **Read-only mode:** When `writable: false`, only `Read`/`Grep`/`Glob` are permitted
- **Parent context:** Optional inheritance from the main session
- **Model override:** Can use a different model profile than the parent

```rust
run_forked_agent(
    api_client,
    "You are a security reviewer.",
    "Review auth.rs for vulnerabilities.",
    Some("Parent: refactoring auth module"),
    Some(&["Read".into(), "Grep".into()]),
    &ModelProfile::Main.resolve(),
    2048,
    false,  // read-only
).await
```

---

## 4. QueryEngine — Turn State Machine

`QueryEngine` (`crates/core/src/engine.rs`) is the core execution kernel. Each turn flows through:

```
1. Context building
   ├── PromptCompiler.assemble() → system prompt + context messages
   ├── Load session messages
   ├── Inject denied tool history
   └── Build tool definitions

2. API request
   ├── Streaming: create_message_streaming()
   └── Non-streaming: create_message()

3. Response handling
   ├── Text delta → emit EngineEvent::TextDelta
   ├── Tool call → schedule tool execution
   └── Stop reason → evaluate continuation

4. Tool execution (if tool_use)
   ├── Permission check (AutoClassifierHook / ExplainPermissionHook)
   ├── Execute tool via ToolExecutor trait
   ├── Tool result → append to messages
   └── Continue to next model call (loop)

5. Continuation evaluation
   ├── Check terminal reason (end_turn, max_tokens, tool_use, etc.)
   ├── Loop detection
   └── Compact if token budget exceeded

6. Finalize
   ├── Update cost tracker
   ├── Emit EngineEvent::SessionEnd
   └── Return TerminalReason
```

### 4.1 EngineEvent Stream

The engine emits events via an `mpsc::unbounded_channel`:

```rust
pub enum EngineEvent {
    TextDelta(String),                       // Streaming text chunk
    ToolCallStart { id, name, summary },    // Tool execution started
    ToolCallEnd { id, result },             // Tool execution completed
    PermissionRequest { id, tool, args },   // User approval required
    SessionEnd,                             // Turn completed
}
```

### 4.2 Token Budget & Compact

```rust
pub struct CompactConfig {
    pub auto_compact_threshold: usize,      // 100_000 tokens
    pub reactive_compact_threshold: usize,  // 180_000 tokens
    pub snip_target_tokens: usize,          // 80_000 tokens
    pub max_recovery_retries: u32,          // 3
}
```

When the token budget is exceeded, the engine triggers compact: older messages are summarized and replaced with a compact summary message.

---

## 5. Permission System

### 5.1 PermissionEngine

`PermissionEngine` (`crates/core/src/permission_engine.rs`) evaluates whether a tool call requires user approval:

```rust
pub struct PermissionEngine {
    checker: PermissionChecker,
    hooks: Vec<Arc<dyn PermissionHook>>,
}
```

**Hooks:**
- `AutoClassifierHook` — AI-based auto-approval classification (`run_lightweight_query`)
- `ExplainPermissionHook` — Generates risk explanation when approval is needed (`run_side_query`)

### 5.2 Permission Modes

| Mode | Behavior |
|------|----------|
| `Default` | Ask for approval on destructive tools |
| `Auto` | Auto-approve based on classifier |
| `Restricted` | Always ask |
| `Yolo` | Auto-approve most actions |

---

## 6. Tool System

### 6.1 Tool Registry (`coderex-tools`)

Tools are registered in a tiered registry:

```rust
pub enum ToolTier {
    Baseline,      // Always available
    FeatureGated,  // Requires feature flag
    DynamicMcp,    // Discovered at runtime via MCP
    Internal,      // Debug/testing
}
```

**Built-in tools** (`crates/tools/src/builtins/`):
- File: `ReadTool`, `WriteTool`, `EditTool`, `GlobTool`, `GrepTool`
- Shell: `BashTool`, `PowerShellTool`
- Web: `WebFetchTool`, `WebSearchTool`
- Project: `TaskCreateTool`, `TaskUpdateTool`, `CronCreateTool`, `TodoWriteTool`
- Dev: `LspTool`, `NotebookEditTool`
- Browser: `ChromeListTabsTool`, `ChromeNavigateTool`, `ChromeExecuteJsTool`, `ChromeScreenshotTool`, `ChromeGetContentTool`
- Meta: `ToolSearchTool`, `EnterPlanModeTool`, `ExitPlanModeTool`

### 6.2 Tool Execution Pipeline

```
QueryEngine → ToolExecutor trait → RegistryToolDispatcher
                                      ├── ToolRegistry lookup
                                      ├── Sandbox policy injection
                                      ├── Shared state injection (task_store, cron_scheduler, rate_limiter, chrome_client)
                                      └── InnerExecutor::execute_single()
```

The `ToolExecutor` trait in `coderex-core` decouples the engine from concrete tool implementations, allowing test doubles and MCP fallback.

### 6.3 MCP Integration

`coderex-mcp` provides MCP server support:
- `McpManager` — server lifecycle management
- `McpToolExecutor` — routes tool calls to MCP servers
- `CompositeToolExecutor` — tries primary registry first, falls back to MCP

---

## 7. Memory System

### 7.1 Session Memory (`coderex-memory`)

AI-driven session fact extraction:

```rust
// crates/memory/src/session_memory_agent.rs
pub async fn update_session_memory_ai(
    api_client: &dyn ApiClient,
    store: &SessionMemoryStore,
    session_id: Uuid,
    current_memory: &str,
    conversation_excerpt: &str,
) -> Result<String, MemoryWorkflowError>
```

- Calls `prompts::update_session_memory()` via `run_forked_agent`
- Persists extracted facts as `SessionFact` structs with category + confidence
- Triggered from REPL via `/update-memory` command

### 7.2 Long-Term Memory

```rust
// crates/memory/src/extract_memories.rs
pub async fn extract_long_term_memories_ai(
    api_client: &dyn ApiClient,
    store: &LongTermStore,
    conversation: &str,
) -> Result<Vec<String>, MemoryWorkflowError>
```

- Calls `prompts::extract_memories()` via lightweight query
- Writes topic-indexed memories to `LongTermStore`
- Triggered from REPL via `/extract-memories` command

### 7.3 Auto-Dream

`AutoDream` (`coderex-memory`) periodically consolidates session transcripts into long-term memory. Runs as a background task with configurable interval (default: every 10 turns or 30 minutes).

---

## 8. Background Features

The REPL spawns background tasks via `Orchestrator::run_background_feature`:

| Feature | Trigger | Description |
|---------|---------|-------------|
| `AgentSummary` | Sub-agent running | 3-5 word progress summary |
| `AwaySummary` | User returns after idle | 1-3 sentence catch-up |
| `PromptSuggestion` | After each turn | Ghost text suggestion |
| `MagicDocs` | `/magic-docs` command | Update documentation |
| `CodebaseOverview` | `/codebase-overview` command | High-level project summary |
| `ReadmeGeneration` | `/generate-readme` command | README generation |
| `SessionMemoryUpdate` | `/update-memory` command | Session fact extraction |
| `MemoryExtraction` | `/extract-memories` command | Long-term memory extraction |

Background tasks are:
- **Cancelable** via `CancellationToken`
- **Observable** — results are delivered back to the UI thread
- **Isolated** — run as forked agents with read-only defaults

---

## 9. Command-Driven Prompt Injection

Commands can modify the AI's behavior for subsequent turns via `CommandResult`:

```rust
pub struct CommandResult {
    pub output: String,
    pub extra_context: Option<String>,          // Inject user context
    pub append_system_prompt: Option<String>,   // Append to system prompt
    pub override_system_prompt: Option<String>, // Full override
    pub prompt_profile: Option<PromptProfile>,  // Switch persona
    pub allowed_tools_override: Option<Vec<String>>, // Restrict tools
    pub state_changed: bool,
}
```

**Example — `/advisor` command:**

```rust
CommandResult::success("Advisor mode activated.")
    .with_prompt_profile(PromptProfile::Advisor)
    .with_append_system_prompt("Focus on architecture advice and trade-offs.")
```

**Example — `/bughunter` command:**

```rust
CommandResult::success("Bug Hunter mode activated.")
    .with_prompt_profile(PromptProfile::BugHunter)
    .with_allowed_tools_override(vec!["Read".into(), "Grep".into()])
```

This allows commands to shape agent behavior without recompiling or restarting.

---

## 10. Chrome MCP Bridge

Chrome DevTools Protocol (CDP) integration provides browser automation tools:

**Connection:** HTTP to `http://localhost:9222` for tab discovery, WebSocket for commands.

**Tools:**
- `chrome_list_tabs` — List open tabs
- `chrome_navigate` — Open URL in new tab
- `chrome_execute_js` — Execute JavaScript (requires approval)
- `chrome_screenshot` — Capture base64 PNG
- `chrome_get_content` — Extract page HTML as markdown

The `ChromeClient` is injected into `ToolContext.extra` via `RegistryToolDispatcher::set_chrome_client()`.

---

## 11. Session Teleport

Session sharing and directory switching:

- **Save/Load:** `TeleportSnapshot` serializes `SessionState` to `.coderex/teleport_state.json`
- **Share:** Generate 8-character alphanumeric tokens stored in `~/.coderex/teleport/shared/`
- **Delta sync:** `StateDelta` computes HashMap diffs for bandwidth-optimized sync
- **REPL commands:**
  - `/teleport <dir>` — Save current session, change CWD, load target session
  - `/teleport --share` — Generate share token
  - `/teleport --join <token>` — Load shared session
  - `/teleport --list` / `--revoke <token>` — Manage shares

---

## 12. AI Call Inventory

| File | Pattern | Model | Purpose |
|------|---------|-------|---------|
| `crates/core/src/engine.rs` | `submit_streaming` | Main | Primary conversation loop |
| `crates/core/src/orchestrator.rs` | `run_main_agent` | Main | Orchestrated main loop |
| `crates/core/src/orchestrator.rs` | `run_side_query` | Main | Permission explain, decisions |
| `crates/core/src/orchestrator.rs` | `run_lightweight_query` | Lightweight | Title, classify, parse |
| `crates/core/src/orchestrator.rs` | `run_forked_agent` | Configurable | Sub-agents, background |
| `crates/core/src/prompts/features/session_title.rs` | `run_lightweight_query` | Lightweight | Session title generation |
| `crates/core/src/prompts/features/permission_explainer.rs` | `run_side_query` | Main | Risk explanation |
| `crates/core/src/prompts/features/compact_summary.rs` | `run_forked_agent` | Main | Context compression |
| `crates/core/src/prompts/features/session_memory.rs` | `run_forked_agent` | Main | Memory file update |
| `crates/core/src/prompts/features/memory_extraction.rs` | `run_forked_agent` | Main | Long-term memory |
| `crates/core/src/prompts/features/prompt_suggestion.rs` | `run_background_feature` | Background | Input ghost text |
| `crates/core/src/prompts/features/agent_summary.rs` | `run_forked_agent` | Background | Progress summary |
| `crates/core/src/prompts/features/agent_generation.rs` | `run_side_query` | Main | Agent config JSON |
| `crates/core/src/prompts/features/magic_docs.rs` | `run_forked_agent` | Background | Documentation |
| `crates/memory/src/session_memory_agent.rs` | `update_session_memory_ai` | Main | Session fact extraction |
| `crates/memory/src/extract_memories.rs` | `extract_long_term_memories_ai` | Lightweight | Memory persistence |
| `crates/cli/src/repl.rs` | `spawn_background_feature` | Background | All background features |

---

## 13. Key Design Decisions

1. **Static/dynamic prompt split** — Maximizes Anthropic prompt cache hits by separating cacheable base instructions from per-turn context.

2. **Orchestrator abstraction** — Business logic expresses intent (`QueryKind::SideQuery`) rather than raw API parameters, enabling model swaps and A/B testing without code changes.

3. **Forked agent isolation** — Sub-agents get independent context windows with tool whitelists and read-only enforcement, preventing runaway background tasks from corrupting the workspace.

4. **Feature-gated prompt modules** — Each AI feature has its own prompt file in `prompts/features/`, avoiding the monolithic prompt file anti-pattern.

5. **ToolExecutor trait** — Decouples the engine from tool implementations, enabling test doubles, MCP fallback, and plugin tools.

6. **CommandResult prompt injection** — Commands shape agent behavior through structured effects rather than string manipulation, making behavior changes predictable and reversible.

---

## 14. Provider Interaction Architecture

This section documents the provider/LLM interaction architecture that is implemented in CodeRex today.

### 14.1 Goals

- Stable tool invocation (prefer structured tool-use events over text pseudo-calls)
- Cross-provider compatibility with a single engine path
- Strong Code Agent identity signals (headers + metadata + session context)
- Observable and recoverable request lifecycle

### 14.2 Layering

```text
QueryEngine
  -> ApiClient::with_request_context(RequestContext)
      -> ProviderRouter
          -> ModelRouter
              -> ProviderAdapter
                  -> reqwest transport + SSE/parser logic
```

- `QueryEngine` (`crates/core/src/engine.rs`): builds provider-facing messages, assembles `RequestContext`, and owns the turn/tool loop.
- `ProviderRouter` (`crates/core/src/provider_router.rs`): explicit routing boundary used by the engine.
- `ModelRouter` (`crates/core/src/model_router.rs`): priority/fallback chain, model filtering, fallback allowlist, and per-request context propagation.
- `ProviderAdapter` (`AnthropicProvider`, `OpenAiCompatibleProvider`): provider-specific request shaping, stream parsing, capability checks, and error normalization.
- Transport is currently implemented inside the adapters (`AnthropicApiClient` / `OpenAiCompatibleProvider`) rather than as a standalone shared `HttpTransport` type.

### 14.3 Unified Internal Request Model

The provider-facing request shape is normalized around:

- `system`
- `messages`
- `tools`
- `max_tokens`
- `stream`
- `RequestContext { session_id, device_id, turn_id, agent_id, request_id, provider_hints }`

`RequestContext` is built in the engine, threaded through `ApiClient::with_request_context`, and consumed by the router/adapters to shape headers, metadata, API style, and diagnostics.

### 14.4 Provider Profile Contract

Provider profiles are resolved by `ResolvedProviderProfile` and currently define:

- `api_style`: `anthropic_messages` | `openai_chat` | `openai_responses`
- base URL + path behavior
- default headers (version/beta/platform headers)
- metadata mapping rules
- tool-call capability flags (structured stream, parallel tools, etc.)
- error mapping policy

Implemented profile families:

- `anthropic`
- `dashscope`
- `kimi`
- `openai`
- `groq`
- `custom`

The capability matrix is represented by `ProviderCapabilities` and is used to suppress unsupported structured-tool paths (for example, Kimi).

### 14.5 Identity and Session Signals

For each main-thread request, CodeRex now standardizes:

- `User-Agent: CodeRex/<version> (<os>; <arch>)`
- `X-CodeRex-Session-Id: <session_id>`
- `X-CodeRex-Client: cli`
- `metadata.user_id` (JSON string containing `device_id`, `session_id`, `turn_id`, `agent_id`, `client`, `request_id`)
- `metadata.session_context` (`session_id`, `turn_id`, `agent_id`)

Additional signals when available:

- `X-CodeRex-Agent-Id`
- `X-CodeRex-Request-Id` (local request correlation id)
- provider-specific headers from the selected profile

DashScope-specific headers are injected for DashScope profiles. Both Anthropic-compatible and OpenAI-compatible paths now emit the same CodeRex identity/session shape.

### 14.6 Tool Calling Path

Primary path (structured):

1. Parse stream events (`text/tool_start/tool_delta/tool_end/usage/stop`)
2. Incrementally assemble tool input (`partial_json`/delta)
3. Execute tools via normal tool scheduler
4. Inject `tool_result` back into conversation and continue

Fallback path (compatibility only):

1. Parse inline text call (e.g. `Read({...})`)
2. Re-validate tool name + schema
3. Execute and inject canonical `tool_result`
4. Record `fallback_reason` for diagnostics/governance

Implemented structured paths:

- Anthropic messages SSE
- OpenAI chat-completions SSE
- OpenAI responses SSE

Implemented compatibility paths:

- pseudo-tool parsing in the engine
- Kimi tool suppression with text-tool fallback
- headless single-read normalization in the CLI for `Read ...` workflows

### 14.6.1 Thinking Blocks (Anthropic-compatible thinking mode)

A provider in thinking mode returns `{"type":"thinking","thinking":"…","signature":"…"}`
before the text and tool_use blocks, and requires that block back on the next
request. Omitting it fails with HTTP 400:

```text
The `content[].thinking` in the thinking mode must be passed back to the API.
```

CodeRex handles the block on both response paths:

- **Non-streaming** — `ApiResponse.thinking` captures `thinking` and `signature`
  from the content-block loop.
- **Streaming** — `content_block_start` with `type: "thinking"` contributes any
  initial fragment; `thinking_delta` and `signature_delta` are accumulated by
  `StreamingToolCallAssembler` and surfaced as one `ThinkingBlock` on the
  completed `StreamedResponse`. `StreamEvent::ThinkingDelta` /
  `StreamEvent::SignatureDelta` carry them.
- **Engine** — after either response the block is stored as
  `MessageContent::Thinking(ThinkingBlock)` before the visible assistant text
  and before tool_use persistence, so it belongs to the same assistant turn.
- **Request building** — `provider_messages` never emits a thinking message on
  its own. It holds the block and prepends it to the next assistant message's
  content array, merging consecutive assistant messages so one provider turn is
  one message: `[thinking, text, tool_use]`. If no assistant message follows,
  the pending block is dropped. Blocks are never synthesized for providers that
  did not return one.

Thinking is provider state, not visible output: export, insights, compaction
summaries, and the brief tool skip it as display text. It is still counted in
token estimates because it is sent back to the provider.

### 14.7 Tool Execution Orchestration

- Concurrency-safe (read-only) tools may run in parallel
- Side-effecting tools should run serially
- Critical tool errors should support coordinated cancellation
- Streaming fallback must discard mismatched/orphan tool results

### 14.8 Error Model and Recovery

Current error normalization is two-layered:

- transport/API layer: `ApiError::{Auth, AccessPolicy, Response, Http, RetryExhausted, InvalidModel}`
- provider/router layer: `ProviderErrorKind::{Auth, AccessPolicy, RateLimit, Retryable, InvalidRequest, Unavailable, Unknown}`

Access-policy failures such as “only available for Coding Agents” are classified explicitly and do not trigger provider fallback.

Recovery order:

1. Retry same provider (backoff)
2. Streaming -> non-streaming fallback (same semantic request)
3. Optional fallback provider (if policy allows)
4. User-facing actionable error with request id

Returned provider error messages now include local and server request identifiers when available.

### 14.9 Observability

Per request, CodeRex now logs/tracks:

- local `request_id` + provider `server_request_id` (if available)
- provider/profile/model/base_url
- fallback flags (`tool_fallback`, `streaming_fallback`)
- tool execution stats (concurrency, failures, cancellations)
- token/latency/cost metrics

Request start / response logs include provider, profile, model, base URL, request IDs, and fallback flags.

### 14.10 Configuration Surface

Implemented configuration surface:

- `provider_profile`
- `custom_headers`
- `metadata_policy`
- `fallback_providers`
- `session_id`
- `device_id`

Primary env sources today:

- `CODEREX_PROVIDER_PROFILE`
- `CODEREX_CUSTOM_HEADERS`
- `CODEREX_FALLBACK_PROVIDERS`
- `CODEREX_SESSION_ID`
- `CODEREX_DEVICE_ID`

Policy: safe defaults, explicit overrides, auditable behavior.

### 14.11 Alignment with Current CodeRex

Implemented:

1. Explicit `ProviderRouter -> ModelRouter -> ProviderAdapter` layering
2. Shared `RequestContext` propagation from engine to transport
3. Standardized `X-CodeRex-Session-Id`, `X-CodeRex-Agent-Id`, and `metadata.user_id`
4. Provider capability matrix for structured tool use / streaming decisions
5. Real `OpenAiResponses` request and stream handling
6. Request/server request IDs surfaced in provider diagnostics
7. Text pseudo-call parsing retained as the compatibility path

### 14.12 Non-Goals

- No bypass/impersonation logic against provider policy
- No provider-specific hardcoded business logic in engine core
- No secret leakage in logs or transcripts
