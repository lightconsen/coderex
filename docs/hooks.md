# hooks Module

## Goal

Lifecycle hook system. Hooks are external scripts that intercept and modify
behaviour at key points — before a tool runs, before a prompt is submitted,
around compaction, at the end of a turn. A hook reads a JSON input on stdin and
prints a JSON result on stdout; a `PreToolUse` hook can deny a tool, a
`UserPromptSubmit` hook can refuse a prompt.

## Key Types

- **`HookRegistry`** (`engine.rs`) — holds hook definitions and matches them to
  events. Implements `HookExecutor`, so it can be installed on the engine.
- **`HookConfig`** (`engine.rs`) — one hook: `event`, `type`, `command`, `env`,
  `cwd`, `timeout_ms`, `priority`, `dry_run`, the `ulimit` limits, and `tier`.
- **`HookTier`** (`engine.rs`) — `Managed` / `Plugin` / `Core`. Decides chain
  order; see below.
- **`HookType`** (`engine.rs`) — the `type` discriminator: `Command`, `Http`,
  `Prompt`, `McpTool`, `Agent`. Only `Command` executes.
- **`HookMatcher`** (`engine.rs`) — matches events by name.
- **`HookInput`** (`engine.rs`) — the JSON written to a hook's stdin.
- **`HookResult`** / **`HookAction`** / **`HookPermissionDecision`**
  (`events.rs`) — what a hook answered.
- **`HookExecutionResult`** (`engine.rs`) — a parsed result plus stdout, stderr,
  exit code.
- **`HookError`** (`events.rs`) — typed failure: `Timeout`, `SpawnFailed`,
  `InputNotDelivered`, `NoOutput`, `UnparseableOutput`, `ExecutionFailed`,
  `InvalidCommand`, `JsonError`.
- **`HookFireResult`** (`crates/core/src/hook_executor.rs`) — the aggregate a
  firing site reads: `stop`, `updated_input`, `errors`, and `failed`.

## Installing hooks (one line)

`Settings.hooks` is raw JSON that nothing used to load. The install path is:

```rust
let registry = coderex_hooks::HookRegistry::from_settings_json(
    settings.hooks.as_ref().unwrap_or(&serde_json::Value::Null),
)?;
engine.set_hooks(registry.into_executor());
```

`from_settings_json` accepts either the bare array that `Settings.hooks` holds
(`[{ "event": ..., "command": ... }]`) or a whole settings object
(`{"hooks": [...]}`), so the caller does not have to know which it has.

`from_settings_json` assigns every hook the `Core` tier. Hooks from an
administrator-controlled source must use `from_settings_json_with_tier(value,
HookTier::Managed)` so they can run ahead of repository-controlled hooks.

**Status:** this API exists and is tested, but the CLI does not yet call it —
`crates/cli/src/repl.rs` still constructs the engine with the default
`NoOpHookExecutor`. Until that one line is wired, hooks are inert in the
production binary no matter how they are configured.

## Chain order: managed → plugin → core

Hooks for an event run in **tier order** — `Managed`, then `Plugin`, then
`Core` — and `priority` (highest first) only breaks ties *within* a tier.
Registration order breaks the remaining ties.

The tier is assigned by the loader from **where the settings came from**, never
from a field in the settings file: `HookConfig.tier` is `#[serde(skip)]`, and a
settings file that writes `"tier": "managed"` has that field ignored. This is a
security property, not cosmetics. Hooks come from repository-controllable
settings, and the chain order decides which hook gets to speak first; if a
repository could set its own tier, it could promote itself ahead of the managed
policy that is supposed to constrain it.

## Hook kinds (`type`)

Claude has five kinds — `command`, `prompt`, `mcp_tool`, `http`, `agent`.
CodeRex has two working kinds and three recognised-but-unimplemented ones:

| `type` | Status |
|--------|--------|
| `command` | **Implemented.** Runs `sh -c <command>` with the input JSON on stdin. |
| `http` | **Implemented.** POSTs the input JSON to `url`; the response body is the answer. See below. |
| `prompt` | Recognised, **skipped with a warning**. Needs a model/API client, which does not belong in this crate. |
| `mcp_tool` | Recognised, **skipped with a warning**. Needs the MCP server registry, which lives in another crate. |
| `agent` | Recognised, **skipped with a warning**. Same reason as `prompt`. |

A hook whose `type` is one of the three unimplemented kinds is **not** run as a
command and **not** silently dropped — the loader logs a `warn!` naming the type
and the event, and the other hooks in the same settings still load. An
unrecognised `type` string (a typo) is a hard config error. A `command` hook
with no `command` field is a hard config error. A hook with no `type` at all
defaults to `command`, so existing settings keep working.

### `http` hooks

```json
{
  "event": "PreToolUse",
  "type": "http",
  "url": "https://policy.example.test/hooks/tool-use",
  "headers": { "authorization": "Bearer ${HOOK_TOKEN}" },
  "allowedEnvVars": ["HOOK_TOKEN"],
  "blockPrivateHosts": true
}
```

The request body is the same JSON a `command` hook receives on stdin, and the
response body is parsed exactly the way `command` stdout is: `{}` or
`{"action":"continue"}` continues, `{"action":"stop"}` stops. The failure rules
are the command hook's rules one for one — a request that does not complete, a
status that is not 2xx, an empty body, and a body that is not a hook result are
all failures, and a failure denies on a gating event.

Three properties are deliberate, because the body is the conversation:

- **The URL must be absolute `http`/`https`.** A relative path or a
  `file:`-style scheme is refused at load: the destination has to be named
  outright.
- **A redirect is a failure.** `3xx` is not followed — following one would send
  the conversation to a host the configuration never named.
- **A header may only read a granted variable.** `$NAME` and `${NAME}` are
  interpolated from the environment, but only for names in `allowedEnvVars`; a
  configuration that references anything else is refused at load, so a hook
  cannot read out a variable simply by naming it. A variable that is named but
  unset is a failure at call time, not an empty substitution.

`blockPrivateHosts` is off by default, because a local hook endpoint is an
ordinary thing to want. On, it refuses an endpoint whose host resolves to
localhost or a private range. Both of those live in
`coderex_core::hook_security` — `UrlValidator`, `SsrfGuard`, `EnvInterpolator`
and `env_var_references` — which existed for this purpose and had no caller
until now.

## Fail-closed contract

Hooks gate work, so a gate that could not be answered must not read as an
allow. A hook is a **failure** — and does not read as "continue" — when any of
these is true:

- **The input could not be delivered.** A stdin write that fails for any reason
  other than a broken pipe means the hook ran without its input, so its answer
  cannot be trusted. A *broken pipe* is deliberately **not** a failure: it means
  the hook closed stdin without reading it — it declined the input, which a hook
  that does not look at stdin legitimately does, and the outcome is racy with
  scheduling. A hook that needed the input and died instead fails on its own,
  via a non-zero exit or empty output.
- **The result could not be retrieved.** Non-zero exit, empty stdout, or stdout
  that is not a hook result. A hook that wants to continue must say so
  explicitly — `{}` or `{"action":"continue"}`. Silence is not consent.
- **The hook could not answer.** A timeout, or a spawn failure.

On a **gating event** — `PreToolUse` (a `stop` denies the tool) and
`UserPromptSubmit` (a `stop` refuses the prompt) — a failure sets both
`HookFireResult.failed` and `HookFireResult.stop`, so the existing firing sites
deny. `HookEvent::is_gating()` is the single source of truth for which events
these are, and a test keeps it in sync with the firing sites. On any other
event there is nowhere to deny, so the failure is only recorded in `errors`.

`failed` distinguishes "the hook said stop" from "the hook could not answer".
Callers that only gate read `stop`; callers that want to tell the two apart read
`failed`.

Note this is **stricter than Claude's default**. Claude's `onFailure` defaults
to `continue` for `command` hooks; CodeRex chooses fail-closed because the
checklist asks for it. A PreToolUse hook that exits non-zero, prints nothing, or
prints non-JSON will deny the tool rather than silently allowing it.

## What actually fires

`HookEvent` has **22 names**, but a name is not a promise. `HookEvent::is_triggered()`
is the single source of truth, and a test pins it.

**Fired (10):** `PreToolUse`, `PostToolUse`, `PostToolUseFailure`,
`UserPromptSubmit`, `PermissionRequest`, `PermissionDenied`, `PreCompact`,
`PostCompact`, `Stop`, `TurnEnd`.

**Reserved, never fired (12):** `SessionStart`, `Setup`, `SubagentStart`,
`Notification`, `CwdChanged`, `FileChanged`, `WorktreeCreate`, `SubagentStop`,
`TaskCompleted`, `TeammateIdle`, `Elicitation`, `ElicitationResult`.

A hook configured on a reserved event matches nothing and runs never. That is
indistinguishable from a working hook until someone notices it did not run, so
the load path warns about each one (`warn_about_inert_hooks`). An unknown event
name is warned about too. Event names come from `HookEvent::as_str`, which for
the events core fires returns core's `hook_events` constants, so the firing side
and the matching side cannot drift.

## Execution model

- **Subprocess.** A `command` hook runs via `sh -c` with the `HookInput` JSON on
  stdin and a `HookResult` JSON on stdout.
- **Timeout.** Each hook is wrapped in `tokio::time::timeout` with
  `timeout_ms` (default 5000).
- **Sync / async.** `execute_sync` runs hooks in chain order, sequentially;
  `execute_async` spawns them as tasks. `fire_hook` uses the sequential path so
  tier/priority order is meaningful.
- **Sandboxing.** Optional `ulimit` limits on CPU time (`max_cpu_seconds`),
  address space (`max_memory_mb`), and file size (`max_file_size_mb`), injected
  into the shell command prefix.
- **Dry-run.** `dry_run: true` executes the hook but suppresses its `stop`
  action — useful for testing a hook without letting it block work.
- **Logging.** Every execution logs `hook_command`, `event`, `duration_ms`,
  `exit_code`, and `error` via `tracing`.

## Worker isolation — a deliberate ⚪ difference

Claude runs plugin function-hooks in dedicated `worker_threads` with a
bidirectional protocol (26 message types plus `SharedArrayBuffer` timestamps).
CodeRex does not, and should not copy this:

- Claude's function-hooks are **in-process** JavaScript modules; a worker thread
  is how Claude isolates them from the host. CodeRex hooks are already a
  **separate OS process** (`sh -c`), which is stronger isolation than a worker
  thread — a worker shares the address space, a subprocess does not.
- The bidirectional protocol exists in Claude to support streaming, `next`
  middleware, and pulling results back from a worker that shares memory. CodeRex
  hooks are one-way request/response by design; there is nothing to stream.
- CodeRex has no in-process JS hook modules to isolate, so a worker would be a
  second, weaker copy of isolation the subprocess boundary already provides.

A bidirectional protocol here would add a lot of surface and buy nothing. The
one-way stdin/stdout contract plus `ulimit` limits is the right shape for this
codebase.

## Current status

**Works:** registry and matching; chain order (tier then priority); the install
API; `command` hooks with stdin/stdout; timeout; sandbox limits; dry-run;
fail-closed failure classification and the deny-on-gate behaviour; inert-event
and unsupported-type warnings; structured logging.

**Not implemented:** the `prompt`, `mcp_tool`, and `agent` hook kinds
(recognised, skipped, warned). Claude's `onFailure` knob (`continue` / `block`),
`args`/exec form, `if` filters, `once`, `async`/`asyncRewake`, `statusMessage`,
and per-event output schemas are also absent — the hook protocol here is the
minimal one, and the fail-closed policy is fixed rather than configurable.
