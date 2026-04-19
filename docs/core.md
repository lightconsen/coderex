# core Module Inspection

## Goal

The foundational engine of CodeRex. Manages LLM API communication, conversation turn state machines, tool call execution, permission checking, token budgeting, context compaction, and structured output validation. This is the brain of the system — everything else orbits around `core`.

## Design

### Key Types

- **`QueryEngine`** (`engine.rs:592`) — The main conversation orchestrator. Manages turns through a 14-state state machine (`TurnState`).
- **`TurnState`** (`engine.rs:289`) — State machine: Initialize → BuildContext → RunPreSubmitHooks → StartRequest → StreamAssistant → ExecuteTools → AwaitPermission → AwaitElicitation → ApplyToolResults → EvaluateContinuation → CompactRecovery → StopHooks → Finalize → Terminated.
- **`TurnRuntime`** (`engine.rs:475`) — Per-turn mutable state: messages, pending tool calls, budget, recovery count, terminal reason.
- **`ApiClient` trait** (`api_client.rs:150`) — Abstraction over LLM providers. `AnthropicApiClient` is the primary impl.
- **`StreamEvent`** (`api_client.rs:113`) — SSE parsing results: TextDelta, ToolCallBlockStart/End, ToolCallInputDelta, UsageSummary, StreamEnd, Error.
- **`StreamingToolCallAssembler`** (`engine.rs:187`) — Accumulates streaming tool call JSON fragments into complete `ToolCall`s.
- **`StructuredOutputValidator`** (`engine.rs:52`) — Custom JSON schema validator (not using jsonschema crate for this).
- **`PermissionEngine`** — Evaluates whether tool calls are allowed, need approval, or denied.
- **`TokenBudget`** — Tracks per-session and per-turn token consumption.
- **`CostTracker`** — Records USD cost per model/request.
- **`CompactConfig` / compact module** — Context window recovery via micro/auto/reactive/snip strategies.

### Architecture Patterns

- **State machine pattern** for turn lifecycle with explicit transitions.
- **Producer-consumer** via `tokio::mpsc::unbounded_channel` for engine events → UI.
- **Trait-based abstraction** for API client, tool executor, hook executor, elicitation handler.
- **SSE streaming parser** with buffer accumulation and stateful `SseParserState` for tool call index mapping.
- **Retry with exponential backoff** for API calls (3 retries, 1s base backoff).
- **Dual-path execution**: `submit()` (blocking) and `submit_streaming()` (streaming with early tool execution).

### Public API Surface

Re-exports 30+ types from submodules: `types`, `state`, `runtime`, `engine`, `api_client`, `budget`, `cost`, `permissions`, `sandbox`, `file_traversal`, `fuzzy_matcher`, `markdown`, `syntax`, `scheduler`, `prompt`, `compact`, `elicitation`, `task`, `worktree`, `team`, `coordinator`, `orchestrator`, `query_profiles`.

## Current Status

### What Works

- Anthropic API client with retry, Bearer auth for non-Anthropic endpoints.
- Full SSE streaming parser with tool call assembly, stress-tested (50 concurrent tool calls, 100KB payloads, fragmented chunks).
- Turn state machine with both blocking and streaming paths.
- Permission engine with Allow/Deny/Ask/AskForSession/Elevated modes.
- Token budget tracking and cost estimation.
- Context compaction with 4 strategies (micro, auto, reactive, snip).
- Transcript persistence (atomic temp-file + rename).
- Structured output validation (basic JSON schema support).
- Elevation flow (temporarily elevates to BypassPermissions).
- Hook integration (UserPromptSubmit, PreToolUse, PostToolUse, PostToolUseFail, PreCompact, PostCompact, TurnEnd).
- Comprehensive tests: 80+ tests covering all major paths.

### What Is Stubbed/Partial

- **`extract_tool_calls`** (`engine.rs:1788`) — Uses text-based JSON heuristic instead of native API tool blocks. Comment admits: "In a production implementation, tool calls come as structured blocks."
- **`Runtime::bootstrap()`** (`runtime.rs:337`) — Spawns a tokio task that just waits for shutdown; the actual event loop is in `Runtime::run()` but `bootstrap()` doesn't wire it up.
- **Tool call execution in non-streaming path** — Delegates to `tool_scheduler::run_tool_calls` which has `#[allow(dead_code)]` on some fields.
- **`handle_elicitation`** (`engine.rs:700`) — Marked `#[allow(dead_code)]`; default handler returns `Cancelled`.
- **Cron scheduling** (`scheduler.rs`) — Wraps `cron` crate but no actual job execution is wired.
- **Team/worktree/coordinator modules** — Exist as types but not integrated into main flow.
- **Syntax highlighting** — `SyntaxHighlighter` exists but not used in TUI.

## Gaps for Production

- **Tool call extraction reliability**: The regex/JSON-heuristic approach will break with complex responses. Should use Anthropic's native `tool_use` blocks.
  → **Suggestion**: Replace `extract_tool_calls` with native `tool_use` block parsing from the SSE stream. The `StreamingToolCallAssembler` already accumulates structured blocks; wire it directly instead of falling back to regex on the raw response text.
- **No connection pooling**: `reqwest::Client` is created fresh per `AnthropicApiClient`.
  → **Suggestion**: Make `AnthropicApiClient` accept an `Arc<reqwest::Client>` or store the client in `Runtime` and clone `Arc` references per engine instance. This reduces TCP handshake overhead and enables connection reuse.
- **SSE parser edge cases**: `find_sse_message_boundary` only checks `\n\n`, not `\r\n\r\n`. No handling of SSE comments (`:` lines).
  → **Suggestion**: Update `find_sse_message_boundary` to scan for both `\n\n` and `\r\n\r\n`. Add a comment-stripping pass that discards lines starting with `:` before parsing field lines.
- **Budget/cost hardcoding**: Model pricing is likely hardcoded or estimated; no dynamic pricing updates.
  → **Suggestion**: Fetch pricing from an API (e.g., Anthropic's pricing endpoint or a static JSON file on a CDN) at startup with a fallback to embedded defaults. Cache the result with a TTL.
- **No request timeout**: API requests have no explicit timeout beyond reqwest defaults.
  → **Suggestion**: Add a configurable `request_timeout` to `ApiClient` (default 120s). Use `reqwest::ClientBuilder::timeout()` or `tokio::time::timeout` around the request future.
- **Compact strategy selection is simplistic**: `select_compact_strategy` uses fixed thresholds.
  → **Suggestion**: Make thresholds configurable per model (different models have different context window sizes). Consider a hybrid strategy that analyzes message importance (e.g., recency + user vs assistant ratio) before choosing.
- **No graceful degradation**: If API is completely down, no fallback to cached/local mode.
  → **Suggestion**: Implement a "local mode" that serves cached responses or runs a smaller local model (via `llama.cpp` or similar) when the primary API is unreachable. Cache the last N successful responses in SQLite.
- **`ToolCallBlockEnd` parses input as Null**: The SSE parser returns `serde_json::Value::Null` for tool call input on block end, relying on accumulated JSON deltas. This is fragile if deltas are malformed.
  → **Suggestion**: In `StreamingToolCallAssembler`, validate the accumulated JSON before emitting the final `ToolCall`. If malformed, emit an `Error` event instead of silently producing a null input.
