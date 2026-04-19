# hooks Module Inspection

## Goal

Lifecycle hook system with 22 events. Allows external scripts or built-in handlers to intercept and modify behavior at key points: user prompt submission, tool use, compaction, turn end, etc.

## Design

### Key Types

- **`HookRegistry`** (`engine.rs`) — Stores hook definitions with matchers.
- **`HookMatcher`** (`engine.rs`) — Matches events by name, tool, path patterns.
- **`HookConfig`** (`engine.rs`) — Hook execution configuration.
- **`HookInput`** (`engine.rs`) — Context passed to hook: event name, tool name, arguments.
- **`HookResult`** (`events.rs`) — Success/stop/error with optional message.
- **`HookPermissionDecision`** (`events.rs`) — Allow/deny/ask from hook.
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

- **Registry with pattern matching** — Hooks can match by glob patterns on tool names or paths.
- **Sync and async execution** — Hooks can be both sync functions and async tasks.
- **Stop propagation** — A hook returning `stop: true` halts the current operation.

## Current Status

### What Works

- Hook registry with pattern-based matching.
- 22 event types defined.
- Stop hook evaluation for compact/recovery decisions.
- `NoOpHookExecutor` for when hooks are disabled.
- Hook result propagation (stop, allow, deny).

### What Is Stubbed/Partial

- **External script hooks** — Registry supports them but no script runner is implemented.
- **Hook chaining order** — No explicit priority/ordering system.
- **Hook timeout** — Hooks can run indefinitely.

## Gaps for Production

- **No hook sandboxing**: External scripts run with full process privileges.
  → **Suggestion**: Execute external hook scripts in a restricted subprocess using `seccomp-bpf` (Linux) or `sandbox-exec` (macOS). Limit filesystem access to a temporary directory. Set `RLIMIT_CPU` and `RLIMIT_AS` via `libc::setrlimit`.
- **No hook performance monitoring**: Slow hooks block the main loop with no warning.
  → **Suggestion**: Wrap each hook execution in `tokio::time::timeout` (default 5s). Log execution duration via `tracing::info!`. Emit a slow-hook warning if execution exceeds 1s.
- **No hook dry-run mode**: Cannot test hooks without affecting live execution.
  → **Suggestion**: Add a `dry_run: bool` flag to `HookConfig`. In dry-run mode, execute the hook but ignore `stop` decisions and log the result without applying side effects. Add a `/hook test <name>` command.
- **Hook debugging is difficult**: No structured logging of hook inputs/outputs.
  → **Suggestion**: Emit structured `tracing` spans for each hook execution with fields: `hook_name`, `event`, `duration_ms`, `result`. Write a summary to a `~/.coderex/hook_debug.log` file when `debug_hooks` is enabled.
- **No hook marketplace**: Users must write hooks from scratch.
  → **Suggestion**: Create a `hooks/` directory in the repo with curated examples (e.g., `pre-commit-style-check.hook`, `auto-format-on-save.hook`). Document the hook API and provide a template generator.
- **Hook errors are silently swallowed**: `let _ = self.fire_hook(...)` ignores failures.
  → **Suggestion**: Change `fire_hook` to return `Result<HookResult, HookError>` and propagate errors to the caller. For non-critical hooks, log the error and continue. For critical hooks (e.g., `PreToolUse`), surface the error to the user.
