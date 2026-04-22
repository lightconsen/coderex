# hooks Module Inspection

## Goal

Lifecycle hook system with 22 events. Allows external scripts or built-in handlers to intercept and modify behavior at key points: user prompt submission, tool use, compaction, turn end, etc.

## Design

### Key Types

- **`HookRegistry`** (`engine.rs`) — Stores hook definitions with matchers. Supports priority-based ordering.
- **`HookMatcher`** (`engine.rs`) — Matches events by name, tool, path patterns.
- **`HookConfig`** (`engine.rs`) — Hook execution configuration: event, command, env, cwd, timeout_ms, priority, dry_run, max_cpu_seconds, max_memory_mb, max_file_size_mb.
- **`HookInput`** (`engine.rs`) — Context passed to hook: event name, tool name, arguments.
- **`HookResult`** (`events.rs`) — Success/stop/error with optional message.
- **`HookPermissionDecision`** (`events.rs`) — Allow/deny/ask from hook.
- **`HookExecutionResult`** (`engine.rs`) — Parsed result + stdout, stderr, exit_code from subprocess.
- **`HookError`** (`events.rs`) — Typed error enum: Timeout, SpawnFailed, JsonError, ExecutionFailed, etc.
- **`HookFireResult`** (`hook_executor.rs`) — Aggregated result from fire_hook: stop, updated_input, errors.
- **`StopHookContext`** / **`StopHookDecision`** (`stop_hook.rs`) — Compact/recovery decision logic.

### Hook Events (22 total)

| Event | When Fired |
|-------|-----------|
| `UserPromptSubmit` | Before sending user message to API |
| `PreToolUse` | Before executing a tool |
| `PostToolUse` | After successful tool execution |
| `PostToolUseFail` | After failed/denied tool execution |
| `PreCompact` | Before context compaction |
| `PostCompact` | After context compaction |
| `TurnEnd` | At end of each turn |
| + 15 more | Various lifecycle points |

### Architecture Patterns

- **Registry with pattern matching** — Hooks can match by event name or wildcard (`*`).
- **Priority ordering** — Hooks sorted by `priority` field (higher runs first) before execution.
- **Sync and async execution** — `execute_sync` (sequential) and `execute_async` (spawned tasks) modes.
- **Stop propagation** — A hook returning `stop: true` halts the current operation.
- **Subprocess execution** — External scripts run via `sh -c` with JSON input on stdin, JSON result on stdout.
- **Timeout enforcement** — Each hook wrapped in `tokio::time::timeout` with configurable `timeout_ms` (default 5000ms).
- **Dry-run mode** — `dry_run: bool` on `HookConfig` executes hooks but suppresses `stop` actions for safe testing.
- **Structured logging** — Every hook execution logged via `tracing::info!`/`tracing::warn!` with fields: `hook_command`, `event`, `duration_ms`, `exit_code`, `stop`, `error`.
- **Error aggregation** — `HookFireResult.errors` collects errors from all matching hooks; callers log via `tracing::warn!`.
- **Resource sandboxing** — Hooks can apply `ulimit` constraints: CPU seconds (`-t`), memory (`-v`), and file size (`-f`) limits injected into the shell command prefix.

## Current Status

### What Works

- Hook registry with pattern-based matching (exact name + wildcard).
- 22 event types defined and implemented.
- External script hooks — subprocess execution via `sh -c` with JSON stdin/stdout.
- Hook timeout — configurable `timeout_ms` (default 5000ms) via `tokio::time::timeout`.
- Hook priority ordering — higher-priority hooks execute first.
- Dry-run mode — `dry_run: bool` on `HookConfig` suppresses stop actions.
- Structured debug logging — `tracing` spans with duration, exit code, error details.
- Error propagation — `HookFireResult.errors` collects errors; all call sites log failures.
- Hook sandboxing — resource limits via `ulimit`: CPU time (`max_cpu_seconds`), memory (`max_memory_mb`), file size (`max_file_size_mb`).
- Stop hook evaluation for compact/recovery decisions.
- `NoOpHookExecutor` for when hooks are disabled.
- Hook result propagation (stop, allow, deny, updated_input).
- 24 tests covering registry, matching, parsing, execution, timeout, and sandbox.

### What Is Stubbed/Partial

*None remaining.*

## Gaps for Production

*None remaining.*

## Remaining Considerations

- **No hook marketplace/examples**: Users write hooks from scratch. Future enhancement: create a `hooks/` directory with curated examples and a template generator.
