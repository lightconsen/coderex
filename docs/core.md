# core Module Inspection

## Goal

The foundational engine of CodeRex. Manages LLM API communication, conversation turn state machines, tool call execution, permission checking, token budgeting, context compaction, and structured output validation. This is the brain of the system — everything else orbits around `core`.

## Design

### Key Types

- **`QueryEngine`** (`engine.rs`) — The main conversation orchestrator. Manages turns through a 14-state state machine (`TurnState`).
- **`TurnState`** (`engine.rs`) — State machine: Initialize -> BuildContext -> RunPreSubmitHooks -> StartRequest -> StreamAssistant -> ExecuteTools -> AwaitPermission -> AwaitElicitation -> ApplyToolResults -> EvaluateContinuation -> CompactRecovery -> StopHooks -> Finalize -> Terminated.
- **`TurnRuntime`** (`engine.rs`) — Per-turn mutable state: messages, pending tool calls, budget, recovery count, terminal reason.
- **`ApiClient` trait** (`api_client.rs`) — Abstraction over LLM providers. `AnthropicApiClient` is the primary impl.
- **`StreamEvent`** (`api_client.rs`) — SSE parsing results: TextDelta, ThinkingDelta, SignatureDelta, ToolCallBlockStart/End, ToolCallInputDelta, UsageSummary, StopReason, StreamEnd, Error.
- **`StreamingToolCallAssembler`** (`engine.rs`) — Accumulates streaming tool call JSON fragments into complete `ToolCall`s, and thinking/signature deltas into the turn's `ThinkingBlock`.
- **`StructuredOutputValidator`** (`engine.rs`) — Custom JSON schema validator.
- **`PermissionEngine`** (`permission_engine.rs`) — Evaluates whether tool calls are allowed, need approval, or denied. Supports hooks (`PermissionHook`, `ExplainPermissionHook`, `AutoApproveHook`, `BackgroundAgentDenyHook`). `evaluate_with_subjects` takes the call's rule subjects (Bash command, file path(s)) from the tool executor.
- **`PermissionRuleSpec`** (`permissions.rs`) — Parsed Claude-style rule (`Tool`, `Tool(glob)`, `Bash(prefix:*)`, `Bash(glob)`); `PermissionChecker::add_spec` compiles it against a `RuleCompileContext { cwd, home }`. `ToolExecutor::rule_subjects` supplies `RuleSubject`s (one per command segment or file, each with raw/absolute/canonical forms); deny/ask match any subject, allow requires every subject to be covered. A subject marked opaque (unmodelled shell construct) can never be auto-approved. `PowerShell`/`pwsh` specifiers use the same command-segment and opaque analysis as `Bash`. Precedence is deny > ask > allow. `ToolExecutor::set_path_verifier` lets file tools re-check the real path of an opened handle, closing the check/open TOCTOU gap.
- **`PermissionManager`** (`permission_manager.rs`) — Higher-level permission rule management with rule groups and persistent rule storage.
- **`ConfirmationBus`** (`confirmation_bus.rs`) — Decoupled confirmation request/response channel for tool call permissions.
- **`TokenBudget`** (`budget.rs`) — Tracks per-session and per-turn token consumption.
- **`CostTracker`** (`cost.rs`) — Records USD cost per model/request. Loads `~/.coderex/pricing.json` with built-in fallbacks.
- **`CompactConfig` / compact module** — Context window recovery via micro/auto/reactive/snip strategies.
- **`ModelRouter`** (`model_router.rs`) — Routes requests across multiple LLM providers with fallback.
- **`ProviderConfig`** (`provider_config.rs`) — Multi-provider configuration with endpoint definitions.
- **`TaskManager`** (`task.rs`) — Background task lifecycle management (`TaskKind`, `TaskState`, `TaskHandle`).
- **`HookExecutor`** / `HookSecurity` — Hook execution with SSRF guard, URL validation, and environment interpolation.
- **`ShellSecurity`** (`shell_security.rs`) — Bash AST parsing and destructive command detection.
- **`WireProtocol` / `WireTransport`** (`wire_protocol.rs`, `wire_transport.rs`) — JSON-RPC wire encoding/decoding with stdio and in-memory transports.
- **`LoopDetector`** (`loop_detector.rs`) — Detects repetitive agent loops and triggers intervention.
- **`StopHook`** (`stop_hook.rs`) — Pre/post-turn hooks for cleanup and state synchronization.
- **`WorktreeManager`** / `TeamManager` / `Coordinator` — Multi-session orchestration, team memory sharing, and coordinator state machine.

### Architecture Patterns

- **State machine pattern** for turn lifecycle with explicit transitions.
- **Producer-consumer** via `tokio::mpsc::unbounded_channel` for engine events -> UI.
- **Trait-based abstraction** for API client, tool executor, hook executor, elicitation handler.
- **SSE streaming parser** with buffer accumulation and stateful tool call index mapping.
- **Retry with exponential backoff** for API calls (3 retries, 1s base backoff).
- **Dual-path execution**: `submit()` (blocking) and `submit_streaming()` (streaming with early tool execution).
- **Multi-provider routing** — `ModelRouter` + `ProviderConfig` with fallback across providers.
- **Wire protocol** — JSON-RPC over stdio or in-memory channels for bridge/remote communication.
- **Security layers** — `PermissionEngine` with hooks, `HookSecurity` with SSRF guard, `ShellSecurity` with Bash AST parsing, `LoopDetector` for repetitive behavior.
- **Hook lifecycle** — `HookExecutor` runs UserPromptSubmit, PreToolUse, PostToolUse, PostToolUseFail, PreCompact, PostCompact, TurnEnd hooks with security validation.
- **Task management** — `TaskManager` tracks background task state, handles, and cancellation.
- **Stop hooks** — `StopHook` ensures cleanup and state sync at turn boundaries.

### Public API Surface

Re-exports 50+ types from 44 submodules: `types`, `state`, `runtime`, `engine`, `api_client`, `budget`, `cost`, `permissions`, `permission_engine`, `permission_manager`, `confirmation_bus`, `sandbox`, `loop_detector`, `shell_security`, `file_traversal`, `fuzzy_matcher`, `markdown`, `syntax`, `scheduler`, `prompt`, `compact`, `stop_hook`, `hook_executor`, `hook_security`, `tool_executor`, `tool_scheduler`, `elicitation`, `task`, `worktree`, `team`, `coordinator`, `team_context`, `orchestrator`, `query_profiles`, `prompts`, `wire_protocol`, `wire_transport`, `wire_adapter`, `provider`, `providers`, `model_router`, `provider_config`.

### Prompt caching

`AnthropicApiClient` marks request prefixes with the Anthropic API's
`cache_control: {"type": "ephemeral"}` breakpoints, so a multi-turn session
reads the conversation it already paid to write instead of re-processing it on
every request.

Breakpoints are attached, in prefix order, by `apply_cache_breakpoints` in
`api_client.rs`:

1. the last `system` block — the string form is converted to a one-block text
   array because a marker can only attach to a block;
2. the last tool definition — the tool list is a prefix that rarely changes;
3. the last content block of the last message;
4. the last content block of the second-to-last message, when the conversation
   has at least two messages.

Items 3 and 4 are a sliding pair: the next request reads the prefix this request
just wrote, while the older breakpoint remains a valid read point. Message
content supplied as a string is converted to a block array for the same reason
as `system`. The total is capped at the API's **four** breakpoints per request;
empty prefixes (blank system text, empty message content, an empty tool list)
are never marked.

The markers are only emitted by the Anthropic-protocol transport
(`AnthropicApiClient`, including the Bedrock/Vertex/cloud shapes that speak the
Messages API). `OpenAiCompatibleProvider` never sends `cache_control`: those
providers reject the field and perform their own automatic caching.

Prompt caching is on by default. It can be disabled with the top-level
`prompt_caching: false` setting in `settings.json` or with
`CODEREX_PROMPT_CACHING=0`; the environment override wins over the setting.
`UsageSummary::cache_read_tokens` / `cache_creation_tokens` parse the
`cache_read_input_tokens` / `cache_creation_input_tokens` response fields, and a
debug log line (`"prompt cache hit"`) is emitted when a response reports a
non-zero read, so a session can be checked.

## Current Status

### What Works

- Anthropic API client with retry, Bearer auth for non-Anthropic endpoints.
- Full SSE streaming parser with tool call assembly, stress-tested (50 concurrent tool calls, 100KB payloads, fragmented chunks).
- Thinking-block round-tripping: `thinking` + `signature` are parsed from non-streaming responses and accumulated from streaming deltas, stored as `MessageContent::Thinking`, and prepended to the matching assistant turn when the next request is built. Nothing is sent unless the provider returned a block; none are synthesized.
- Turn state machine with both blocking and streaming paths.
- Permission engine with Allow/Deny/Ask/AllowForPath/Elevated results, hooks, and `PermissionManager` rule groups. Session-wide approval is recorded with `record_session_approval`.
- Token budget tracking and cost estimation with `~/.coderex/pricing.json` loading.
- Context compaction with 4 strategies (micro, auto, reactive, snip).
- Transcript persistence (atomic temp-file + rename).
- Structured output validation (JSON schema support).
- Elevation flow (temporarily elevates to BypassPermissions).
- Hook integration (UserPromptSubmit, PreToolUse, PostToolUse, PostToolUseFail, PreCompact, PostCompact, TurnEnd) with SSRF guard and URL validation.
- Elicitation handling with `ReplElicitationHandler` (stdin options) and `SdkElicitationHandler` (event emission).
- Multi-provider routing via `ModelRouter` with `ProviderConfig` endpoint definitions.
- Wire protocol (JSON-RPC) with stdio and in-memory transports for bridge communication.
- Bash AST parsing and destructive command detection (`ShellSecurity`).
- Loop detection for repetitive agent behavior.
- Task management with `TaskManager` for background task lifecycle.
- Team memory sharing, worktree management, and coordinator state machine.
- Orphaned permission recovery on startup with UI event emission.
- `TerminalReason::ApiUnavailable` with friendly UI message; cached last response.
- `ExplainPermissionHook` emits `EngineEvent::PermissionExplanation` via event channel.
- Cron scheduling (`CronRunner`) with background tokio task and job management APIs.
- Syntax highlighting integrated into TUI for assistant code blocks.
- Graceful degradation with API unavailability handling.
- `Runtime::shutdown()` aborts background task cleanly.
- Comprehensive tests: 681 library tests plus the engine/integration suites.

### What Is Stubbed/Partial

No stubs. The list of accepted-but-inert surfaces lives in `docs/tools.md`, `docs/plugins.md`, and `docs/mcp.md`; this section is not an absolute claim.

### Gaps for Production

No known production gaps in the core loop itself. Documented platform and library-only boundaries are in `docs/sandbox.md` and `docs/bridge.md`.

## Recently Fixed

All 21 previously identified items have been resolved: SSE parser edge cases, API request timeouts, tool call extraction reliability, `Runtime::bootstrap()` stub, connection pooling, tool call validation, compact strategy thresholds, scheduler wiring, elicitation heuristics, cron scheduling, syntax highlighting, team/worktree/coordinator integration, pricing file loading, graceful degradation, dead code removal, elicitation handler implementations, ModelRouter wiring, cost summary usage, permission explanation events, and orphaned permission recovery UI.
