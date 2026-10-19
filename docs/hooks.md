# hooks Module

## Goal

Lifecycle hook system. Hooks are external scripts that intercept and modify
behaviour at key points — before a tool runs, before a prompt is submitted,
around compaction, at the end of a turn. A hook reads a JSON input on stdin and
prints a JSON result on stdout; a `PreToolUse` hook can deny a tool, a
`UserPromptSubmit` hook can refuse a prompt.

## Key Types

- **`HookRegistry`** (`engine.rs`) — holds hook definitions and matches them to
  events. Implements `HookExecutor`, so it can be installed on the engine. It
  also carries the injected [`HookActionRunner`], if the session supplied one.
- **`HookConfig`** (`engine.rs`) — one hook: `event`, `type`, `command`,
  `http`, `prompt`, `model`, `mcp`, `env`, `cwd`, `timeout_ms`, `priority`,
  `dry_run`, `stream`, the resource limits, the OS `sandbox` policy, and
  `tier`.
- **`HookSandboxConfig`** (`sandbox.rs`) — per-hook filesystem/network
  policy: `enabled`, `allow_network`, and extra read/write/exec paths. See
  [OS sandbox for command hooks](#os-sandbox-for-command-hooks).
- **`HookTier`** (`engine.rs`) — `Managed` / `Plugin` / `Core`. Decides chain
  order; see below.
- **`HookType`** (`engine.rs`) — the `type` discriminator: `Command`, `Http`,
  `Prompt`, `McpTool`, `Agent`. All five are implemented.
- **`HookActionRunner`** (`action.rs`) — what the session must inject for
  `prompt`, `agent`, and `mcp_tool` hooks: one query answered, or one MCP tool
  called. See below.
- **`ActionPrompt`** / **`McpToolHook`** (`action.rs`) — the request a runner
  receives, and an `mcp_tool` hook's target (`server`, `tool`, `input`).
- **`HookMatcher`** (`engine.rs`) — matches events by name.
- **`HookInput`** (`engine.rs`) — the JSON written to a hook's stdin.
- **`HookResult`** / **`HookAction`** / **`HookPermissionDecision`**
  (`events.rs`) — what a hook answered.
- **`HookExecutionResult`** (`engine.rs`) — a parsed result plus stdout, stderr,
  exit code, and the progress events the hook emitted before its answer.
- **`HookError`** (`events.rs`) — typed failure: `Timeout`, `SpawnFailed`,
  `InputNotDelivered`, `NoOutput`, `UnparseableOutput`, `OutputTooLarge`,
  `ExecutionFailed`, `InvalidCommand`, `JsonError`, `InvalidPromptHook`,
  `InvalidMcpToolHook`, `InterpolationFailed`, `MissingActionRunner`.
- **`HookRequestHandler`** / **`HookHostRequest`** / **`HookStreamEvent`**
  (`stream.rs`) — the bidirectional protocol: a `stream: true` hook may ask the
  host a question while it runs, and may report progress before its answer.
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
`from_settings_sources` loads several `(tier, document)` pairs into one
registry, which is what the CLI uses.

**Status:** wired. `Settings` carries the hook layers separately (`HookLayers`
in `coderex-config`: core, managed, and plugin), so `crates/cli/src/repl.rs`
loads managed hooks as `HookTier::Managed` and user/project hooks as
`HookTier::Core` in `apply_cli_config`. Managed hooks are not also installed as
Core. Active plugins are the source for the `Plugin` tier: each manifest's
`hooks` value is normalized and merged by `PluginManager::plugin_hooks()`, and
the orchestrator installs the result as `HookLayers.plugin` before the REPL
loads the tiers. A session that wants the `prompt`,
`agent`, and `mcp_tool` kinds must additionally call `set_action_runner` before
`into_executor` (see the kinds section); without it those three kinds fail when
they fire, which is a denial on a gating event.

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

Claude has five kinds — `command`, `prompt`, `mcp_tool`, `http`, `agent` — and
all five are implemented:

| `type` | Status |
|--------|--------|
| `command` | **Implemented.** Runs `sh -c <command>` with the input JSON on stdin. |
| `http` | **Implemented.** POSTs the input JSON to `url`; the response body is the answer. See below. |
| `prompt` | **Implemented.** One query to a model; its text answer is the hook's answer. Needs a runner. |
| `agent` | **Implemented.** The same call with a verifier framing, a cheap model by default, and a longer timeout. Needs a runner. |
| `mcp_tool` | **Implemented.** Calls an MCP tool on a named server. Needs a runner. |

An unrecognised `type` string (a typo) is a hard config error. A hook missing
the field its kind requires — no `command`, no `url`, no `prompt`, no
`server`/`tool` — is a hard config error too. A hook with no `type` at all
defaults to `command`, so existing settings keep working.

### `prompt`, `agent`, and `mcp_tool` hooks

These three kinds need something this crate deliberately does not have: a model
client (`prompt`, `agent`) or the MCP server registry (`mcp_tool`).
`coderex-core`'s API client and `coderex-mcp` are the *session's*, not the hook
system's, so the session injects one implementation:

```rust
let mut registry = coderex_hooks::HookRegistry::from_settings_json(hooks)?;
registry.set_action_runner(std::sync::Arc::new(SessionHookActionRunner::new(api, mcp)));
engine.set_hooks(registry.into_executor());
```

`HookActionRunner` has two entry points: `answer(&ActionPrompt)` for one query,
and `call_mcp_tool(server, tool, input)` for one MCP call. The runner returns the
hook's answer as text, and that text is parsed with the **same** parser a
`command` hook's stdout goes through — `{}` or `{"action":"continue"}` continues,
`{"action":"stop"}` stops. So all five kinds share one answer format, and an
empty or unparseable answer is a failure in all five.

```json
{ "event": "PreToolUse", "type": "prompt", "prompt": "Is this call safe? Here it is: $ARGUMENTS" }
{ "event": "PreToolUse", "type": "agent", "prompt": "Verify this edit is complete: $ARGUMENTS", "model": "claude-sonnet-4-5" }
{ "event": "PreToolUse", "type": "mcp_tool", "server": "policy", "tool": "check_path",
  "input": { "path": "${tool_input.file_path}" } }
```

- **`$ARGUMENTS`** (or `${ARGUMENTS}`) in a `prompt` or `agent` prompt is
  replaced with the hook input JSON — the same JSON a `command` hook receives on
  stdin — so the hook can see what it is gating.
- **`${tool_input.<path>}`** in an `mcp_tool` `input` resolves against the hook
  input's `input` field, parsed as JSON. A path that does not resolve — because
  the `input` field is absent, is not JSON, or the path does not exist — is a
  **failure**, never an empty string: a hook must not silently call a tool with
  a missing argument.
- **Defaults differ by kind, on purpose.** `prompt` gets the session's model
  (`model` is `None`) and a 5 s timeout; `agent` gets
  `AGENT_DEFAULT_MODEL` (`claude-haiku-4-5-20251001`, the cheap one) and
  `AGENT_DEFAULT_TIMEOUT_MS` (60 s), because a verifier that reads a workspace
  runs longer than one question. An explicit `model`/`timeout_ms` wins for both.

**No runner installed is a failure, not a skip.** A configured `prompt`,
`agent`, or `mcp_tool` hook on a session that cannot run it produces
`HookError::MissingActionRunner`, whose message says which kind could not run
and what to call. On a gating event that failure becomes a deny — the whole
point being that a gate this session cannot run must not read as an allow. The
alternative (skipping the hook with a warning) would turn a configured policy
into an open gate.

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

### Fields accepted but not implemented

`if`, `once`, `statusMessage`, and `continueOnBlock` are accepted in settings
and have **no effect**; the loader logs a `warn!` naming them when a
configuration sets one. They are a separate axis from the hook kind — `command`
and `http` do not implement them either — so they are out of scope rather than
half-wired per kind. A settings file written for the reference therefore still
loads; it just does not get filtering, run-once, or the status line.

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
- **The hook could not answer.** A timeout, or a spawn failure. For `prompt`,
  `agent`, and `mcp_tool` hooks, also: **no runner is installed** for the
  session (`MissingActionRunner`), and — for `mcp_tool` — a `${tool_input.*}`
  reference that does not resolve.

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
  stdin and a `HookResult` JSON on stdout. Unless the hook sets `stream: true`,
  stdin is closed after the input, so a hook that reads stdin to EOF (for
  example `sys.stdin.read()`) sees that EOF.
- **Timeout.** Each hook is wrapped in `tokio::time::timeout` with
  `timeout_ms` (default 5000). A timed-out hook is killed (`kill_on_drop`);
  with `stream: true` a best-effort `{"type":"cancel"}` is written first.
- **Sync / async.** `execute_sync` runs hooks in chain order, sequentially;
  `execute_async` spawns them as tasks. `fire_hook` uses the sequential path so
  tier/priority order is meaningful.
- **Resource limits.** `max_cpu_seconds` (`RLIMIT_CPU`) and
  `max_file_size_mb` (`RLIMIT_FSIZE`) are installed in `pre_exec`, before the
  hook program runs, so `ulimit -f unlimited` inside the hook cannot raise
  them. On Linux `max_memory_mb` installs `RLIMIT_AS` **and** creates a cgroup
  v2 group (`memory.max`, `pids.max`) that the hook is attached to after spawn;
  if the cgroup tree is unwritable or not delegated, the hook logs a warning and
  keeps `RLIMIT_AS` only. On macOS `max_memory_mb` attempts `RLIMIT_DATA` as a
  best-effort ceiling — macOS rejects `RLIMIT_AS`, does not enforce `RLIMIT_RSS`,
  and current Darwin returns `EINVAL` for `RLIMIT_DATA` too, so it is not a
  guarantee and not a full address-space cap. Windows uses a Job Object through
  `coderex_core::sandbox` (limits only; see the platform table below).
- **OS sandbox.** A `command` hook is additionally confined with the same
  primitives the shell tools use: `sandbox-exec` on macOS; `bubblewrap` on
  Linux, falling back to Landlock when `bwrap` is unavailable. On Windows the
  hook spawn path cannot attach the AppContainer token, so hooks remain
  unconfined and fail closed on a gate. See the next section.
- **Streaming protocol.** With `stream: true`, stdout is line-delimited JSON.
  A line without a control `type` is the answer (the historical contract,
  unchanged). Before it a hook may emit `{"type":"log",...}`,
  `{"type":"event",...}`, and `{"type":"request","id":N,"method":...}`. The
  host answers the request on stdin with `{"type":"response","id":N,...}` —
  through the session's `HookRequestHandler`, or with `-32601` when none is
  installed. The production CLI installs one: `"context"` returns
  `{cwd, session_id, model, permission_mode}`, `"permission_mode"` returns the
  current mode string, and every other method is refused with `-32601`. It is
  read-only and exposes no environment variables or credentials. The first line
  on stdin is always the `HookInput`.
- **Output bounds.** One stdout line is capped at 1 MiB and the whole output at
  4 MiB; stderr is drained completely but only the first 1 MiB is kept. A hook
  cannot exhaust the host's memory through a pipe.
- **Dry-run.** `dry_run: true` executes the hook but suppresses its `stop`
  action — useful for testing a hook without letting it block work.
- **Logging.** Every execution logs `hook_command`, `event`, `duration_ms`,
  `exit_code`, and `error` via `tracing`; protocol `log` and `event` lines are
  logged as they arrive.

## OS sandbox for command hooks

On macOS and Linux, every `command` hook runs inside the platform sandbox, in
addition to its resource limits. The layer is `coderex_core::sandbox` — the same
`SandboxHandle`, Seatbelt profile, bubblewrap argument builder and Landlock
ruleset the Bash and PowerShell tools use — so there is one policy
implementation, not two.

Windows uses the same backend: `prepare` creates the child suspended and
`finish_windows_spawn` attaches the AppContainer token, assigns the Job Object
and resumes it. The hook command is run through `cmd.exe /D /S /C` (there is no
`sh` and no wrapper program). `max_file_size_mb` has no Windows equivalent and
is warned about; CPU and memory limits live in the Job Object because there is
no `pre_exec`.

### Default policy

| Access | Default |
|---|---|
| Read | The hook's workspace (`cwd`, else the session's working directory), the temp directory, the user's home and `XDG_CONFIG_HOME`, system roots, and the absolute directories on `PATH` (with the prefix when the entry is a `bin`/`sbin` directory) |
| Write | The workspace and the temp directory — **nothing else** |
| Execute | System shells and tools under `/bin`, `/usr/bin`, `/usr/local/bin` (plus `/Library/Developer` on macOS and Homebrew/Toolchain `PATH` entries) |
| Network | Denied |

On macOS this is a `(deny default)` Seatbelt profile: reads of file contents
outside the policy paths are refused (metadata and the root listing are
allowed, as for the shell tools), writes outside the write set are refused,
and `(deny network*)` is emitted. On Linux with `bwrap` this is a mount
namespace: the root is bound read-only, only the write set is re-bound
writable, and `--unshare-net` denies the network. Without `bwrap` the same
policy becomes a Landlock ruleset: read/execute on the policy paths and system
roots, write on the write set (plus `/dev/null`), and — on ABI 4 or newer —
`BIND_TCP`/`CONNECT_TCP` denied unless `allow_network` is set. On older ABIs the
seccomp table installed alongside Landlock is what removes the network syscalls.
Landlock denies access, not path existence: a path outside the ruleset may still
be `stat`-ed.

`HOME` is readable but not writable. That keeps hooks that read
`~/.gitconfig` or a config directory working without granting them the home
directory as a write target.

### Per-hook configuration

The `sandbox` key is optional. Additional paths **extend** the defaults; a
missing key means the default policy.

```json
{
  "event": "PreToolUse",
  "command": "check-policy --input /srv/policy/rules.json",
  "sandbox": {
    "allow_network": false,
    "allowed_read_paths": ["/srv/policy"],
    "allowed_write_paths": ["/tmp/hook-output"],
    "allowed_exec_paths": ["/srv/policy/bin"]
  }
}
```

- `enabled` (default `true`) — `false` disables the OS sandbox for this hook.
  Resource limits still apply.
- `allow_network` (default `false`) — permits network access.
- `allowed_read_paths` / `allowed_write_paths` / `allowed_exec_paths` — extra
  absolute paths. A relative path is refused at load, because it would resolve
  against the workspace and confine nothing.
- `"sandbox": false` and `"sandbox": true` are shorthand for
  `{"enabled": false}` and `{"enabled": true}`.
- An unknown key inside the object is a **load error**, not a silently ignored
  option.

`CODEREX_HOOK_SANDBOX=0` (or `false`/`no`) is the process-wide escape hatch:
every command hook runs with resource limits but without OS confinement. As
with `CODEREX_BWRAP`, only an explicit negative switches it off.

### When the platform cannot confine

The sandbox is not pretended. If no backend can confine — Windows has no
AppContainer runtime, Linux has neither `bwrap` nor
Landlock (or Landlock is switched off with `CODEREX_LANDLOCK=0`), macOS has no
`sandbox-exec` — then:

- On a **gating event** (`PreToolUse`, `UserPromptSubmit`) the hook fails with
  `HookError::SandboxUnavailable`, which is a failure and therefore a deny. A
  configured gate that cannot be enforced must not read as an allow.
- On any **other event** there is nowhere to deny, so the reason is logged at
  `warn` and the hook runs with resource limits only.

An explicit opt-out (`sandbox.enabled: false` or `CODEREX_HOOK_SANDBOX=0`) is
not a failure: it is logged and the hook runs. This is the supported way to
run hooks on a platform without confinement.

### What is enforced, per platform

| Platform | Filesystem | Network | Resource limits |
|---|---|---|---|
| macOS | **Kernel** — Seatbelt read/write paths; gating hooks fail closed without `sandbox-exec` | **Kernel** — `deny network*` | CPU/file-size; `RLIMIT_DATA` attempted best-effort (current Darwin rejects it) |
| Linux with `bwrap` | **Kernel** — read-only root, writable policy paths | **Kernel** — network namespace | CPU/file-size/address space; cgroup v2 memory when the tree is writable |
| Linux without `bwrap`, with Landlock | **Kernel** — Landlock read/write/execute path rules | **Kernel** — Landlock ABI 4+; seccomp table on older ABIs | CPU/file-size/address space; cgroup v2 memory when the tree is writable |
| Linux with neither | **None** — gating hooks fail closed | None (seccomp `EPERM` for denied syscalls only) | CPU/file-size/address space; cgroup v2 memory when the tree is writable |
| Windows | **Kernel** — AppContainer DACL + Low mandatory integrity labels; a failed attach terminates the child and a gating hook fails closed | **Kernel** — no `internetClient` capability unless the hook allows it | Job Object CPU/memory; no file-size limit |
| Other | **None** — gating hooks fail closed | None | — |

The streaming protocol is unaffected: stdin/stdout/stderr are inherited
through the wrapper, so `stream: true` hooks keep their line-delimited
protocol, and the one-way contract still closes stdin after the input. The
macOS profile file and the Windows Job Object are held until the child exits.

**Verification status.** On macOS (Darwin 24.6), the 13 hook-sandbox tests pass;
`macos_memory_ceiling_constrains_a_memory_hogging_hook` reports that current
Darwin rejects `RLIMIT_DATA` with `EINVAL` and skips enforcement rather than
asserting a ceiling. In a Linux `rust:1.94` container (no `bwrap`,
`--security-opt seccomp=unconfined`), the Linux-gated Landlock test runs a real
hook and asserts a write outside its allowed paths is refused. The cgroup tests
skip with an explicit message when `/sys/fs/cgroup` is read-only or the needed
controllers are not delegated; that is the normal container case. The Windows
row is compiled only on Windows and has not been run here.

## Worker isolation

Claude runs plugin function-hooks in dedicated `worker_threads` with a
bidirectional protocol (26 message types plus `SharedArrayBuffer` timestamps).
CodeRex does not copy the thread: its hooks are already a **separate OS
process**, which is stronger isolation than a worker thread — a worker shares
the address space, a subprocess does not. The two capabilities that protocol
provides are implemented in process-appropriate form instead:

- **Kernel-enforced limits.** CPU, file size and (on Linux) address space are
  set in `pre_exec`, so they are part of the child's credentials before exec and
  cannot be raised by the hook itself. Windows uses a Job Object. This replaces
  the old `ulimit` shell prefix, which a hook could undo.
- **Bidirectional streaming.** A `stream: true` hook can emit progress events,
  log lines, and requests; the host answers requests on stdin and may send a
  best-effort `cancel`. The protocol is line-delimited JSON, so any language can
  speak it. Output is read incrementally with line and total-size caps and
  stderr is drained concurrently, so a hook cannot deadlock the host on a full
  pipe or exhaust its memory.

**OS confinement is applied** to `command` hooks: `sandbox-exec` on macOS and
bubblewrap-or-Landlock on Linux, reusing `coderex_core::sandbox` (see
[OS sandbox for command hooks](#os-sandbox-for-command-hooks)). On Windows the
hook is created suspended and finished like a shell command, so it gets the
same AppContainer and Job Object. A gating hook whose confinement cannot be
attached fails closed rather than running unconfined. An explicit
per-hook or environment opt-out is available.

## Current status

**Works:** registry and matching; chain order (tier then priority); the install
API; all five hook kinds — `command` (stdin/stdout), `http` (POST), `prompt`
and `agent` (one model query through an injected runner), and `mcp_tool` (one
MCP call through the same runner); timeout with process kill; kernel-enforced
CPU/file-size/address-space limits (plus a Linux cgroup v2 memory cap when the
tree is writable) and OS filesystem/network confinement for `command`
subprocesses (with per-hook policy and explicit opt-outs); the
`stream: true` protocol (events, logs, host-answered requests, best-effort
cancel, bounded output); dry-run; fail-closed failure classification and the
deny-on-gate behaviour; inert-event warnings; structured logging.

**Not implemented:** Claude's `onFailure` knob (`continue` / `block`),
`args`/exec form, `if` filters, `once`, `async`/`asyncRewake`, `statusMessage`,
`continueOnBlock`, and per-event output schemas — the hook protocol here is the
minimal one, and the fail-closed policy is fixed rather than configurable. The
four settings fields among those that a configuration can write are accepted
and inert, with a load-time warning (see above). The action kinds also have no
`cwd`/`env`/resource-limit application: those are subprocess knobs, and no
subprocess runs. `agent` is the same one-shot call as `prompt` with different defaults and
a verifier framing rather than a multi-turn tool loop.
