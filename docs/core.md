# core Module Inspection

## Goal

The foundational engine of CodeRex. Manages LLM API communication, conversation turn state machines, tool call execution, permission checking, token budgeting, context compaction, and structured output validation. This is the brain of the system — everything else orbits around `core`.

## Design

### Key Types

- **`QueryEngine`** (`engine.rs:592`) — The main conversation orchestrator. Manages turns through a 14-state state machine (`TurnState`).
- **`TurnState`** (`engine.rs:289`) — State machine: Initialize -> BuildContext -> RunPreSubmitHooks -> StartRequest -> StreamAssistant -> ExecuteTools -> AwaitPermission -> AwaitElicitation -> ApplyToolResults -> EvaluateContinuation -> CompactRecovery -> StopHooks -> Finalize -> Terminated.
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
- **Producer-consumer** via `tokio::mpsc::unbounded_channel` for engine events -> UI.
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

*None remaining.*

### Gaps for Production

*None remaining.*

## Recently Fixed

| # | Item | Fix |
|---|------|-----|
| 1 | SSE parser edge cases | `find_sse_message_boundary` handles `\r\n\r\n` and `\n\n`; comment lines (`:`) filtered |
| 2 | No API request timeout | `request_timeout_secs` added to `ApiConfig` (default 120s) |
| 3 | Tool call extraction reliability | Non-streaming path parses native `tool_use` blocks; streaming path validates accumulated JSON |
| 4 | `Runtime::bootstrap()` stub | Now spawns `Runtime::run()` in a background task |
| 5 | No connection pooling | `AnthropicApiClient::new_with_client()` accepts `Arc<reqwest::Client>` |
| 6 | `ToolCallBlockEnd` null JSON | `StreamingToolCallAssembler` validates accumulated JSON before emitting |
| 7 | Compact strategy fixed thresholds | `select_compact_strategy` uses model-relative percentages (auto=50%, reactive=90%) |
| 8 | Tool scheduler dead_code | `run_tool_calls` wired into `execute_tool_calls`; stale `#[allow(dead_code)]` removed |
| 9 | `handle_elicitation` heuristic | Question-mark + short-message trigger wired into both blocking and streaming paths |
| 10 | Cron scheduling | `CronRunner` with background tokio task, job add/remove/list/start/stop APIs added |
| 11 | Syntax highlighting unused | `SyntaxHighlighter` integrated into TUI for assistant code blocks |
| 12 | Team/worktree/coordinator orphan modules | Light integration: `team_context` + `coordinator` in `QueryEngine`, `worktree_manager` in `Runtime` |
| 13 | Budget/cost hardcoding | `CostTracker` loads `~/.coderex/pricing.json` with fallback to built-in defaults; `reload_pricing()` for runtime updates |
| 14 | No graceful degradation | `TerminalReason::ApiUnavailable` variant with friendly UI message; last successful response cached in `QueryEngine` |
| 15 | `extract_tool_calls` dead code | Removed unused fallback function; native `ApiResponse.tool_calls` is the sole path |
| 16 | `Runtime.task` dead code | Added `Runtime::shutdown()` that aborts the background task; removed `#[allow(dead_code)]` |
| 17 | Elicitation handlers stubbed | `ReplElicitationHandler` now reads from stdin with numbered options; `SdkElicitationHandler` emits `EngineEvent::ElicitationRequest` |
| 18 | ModelRouter not wired | CLI `try_create_api_client()` now uses `ProviderConfig::from_env().build_router()` with multi-provider fallback |
| 19 | `CostTracker::format_summary()` unused | `/cost` command now uses `QueryEngine::cost_summary()` → `CostTracker::format_summary()` when available |
| 20 | `ExplainPermissionHook` only traced | Now emits `EngineEvent::PermissionExplanation` via event channel; `TranscriptEntry::PermissionExplanation` added to `UiState` |
| 21 | Orphaned permission recovery UI gap | `PermissionEngine::recover_orphaned()` now returns recovered requests; `QueryEngine::recover_orphaned_permissions()` emits them as `EngineEvent::PermissionRequest`; REPL sets persist path and auto-recovers on startup |
