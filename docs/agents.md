# agents Module

## Goal

Sub-agent system for spawning autonomous worker agents, dream agents (background summarization), and forked contexts. Enables parallel task execution and delegated work with full lifecycle tracking, observability, and retry support.

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                      coderex-agents                              │
├──────────────────────────────────────────────────────────────────┤
│                                                                  │
│  AgentManager                                                     │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │  spawn_agent() ─────────────────────────────────────────┐  │  │
│  │  │                                                       │  │  │
│  │  ▼                                                       │  │  │
│  │  ┌──────────┐  ┌──────────────┐  ┌───────────────────┐  │  │  │
│  │  │ForkedCtx │  │ToolExecutor  │  │  QueryEngine      │  │  │  │
│  │  │(context  │  │Factory       │  │  (coderex-core)   │  │  │  │
│  │  │ inherit) │  │(sandbox,     │  │                   │  │  │  │
│  │  │          │  │ whitelist)   │  │  Full LLM loop    │  │  │  │
│  │  └──────────┘  └──────────────┘  └───────────────────┘  │  │  │
│  │                                                          │  │  │
│  │  ┌──────────────────────────────────────────────────┐   │  │  │
│  │  │  run_agent_task() — per-attempt loop:            │   │  │  │
│  │  │   1. Drain parent messages → prompt              │   │  │  │
│  │  │   2. engine.submit() with timeout                │   │  │  │
│  │  │   3. Collect events → broadcast + NDJSON log     │   │  │  │
│  │  │   4. Validate output against JSON schema         │   │  │  │
│  │  │   5. Retry with escalation prompt on failure     │   │  │  │
│  │  │   6. Write output + meta.json (tokens, status)   │   │  │  │
│  │  │   7. Emit lifecycle event to parent engine       │   │  │  │
│  │  └──────────────────────────────────────────────────┘   │  │  │
│  │                                                          │  │  │
│  │  Observability Pipeline:                                 │  │  │
│  │   engine events ──► broadcast<AgentObservableEvent>      │  │  │
│  │                   ──► NDJSON log (~/.coderex/agent_logs) │  │  │
│  │                   ──► parent EngineEvent channel         │  │  │
│  └────────────────────────────────────────────────────────────┘  │
│                                                                  │
│  Helper Functions:                                               │
│   • run_forked_agent()        — delegate to core orchestrator    │
│   • run_background_feature()  — background AI feature via core   │
│                                                                  │
│  (execute_via_agent/spawn_dream_agent were removed: no caller.)  │
└──────────────────────────────────────────────────────────────────┘
```

## Key Types

### Agent Types (`types.rs`)

- **`AgentType`** — Built-in agent variants: `GeneralPurpose`, `Explore { thoroughness }`, `Plan`, `Verification`, `ClaudeCodeGuide`, `StatuslineSetup`, `Custom { prompt }`.
- **`Thoroughness`** — Search depth for Explore agents: `Quick`, `Medium`, `VeryThorough`.

### Custom Agents (`custom_agents.rs`)

- **`CustomAgentDef`** — User-defined agent loaded from `~/.coderex/agents/*.json` with fields: `name`, `system_prompt`, `allowed_tools`, `model`, `max_tokens`, `output_schema`.
- **`load_custom_agents()`** — Scans `~/.coderex/agents/` and returns a name-to-definition map. Files that fail to parse are skipped (with a warning) rather than failing the session.
- **`load_custom_agents_from_dir(dir)`** — The same scan against an explicit directory, used by tests and embedders that keep definitions elsewhere.
- **`get_custom_agent()`** — Lookup helper for a named custom agent.
- Definitions are loaded when `AgentManager::standalone()` is created and can be replaced with `set_custom_agents()` / extended with `register_custom_agent()`. `custom_agent_names()` returns the sorted names used in unknown-name errors.

### Agent Manager (`manager.rs`)

#### Core Types

- **`AgentManager`** — Central manager: spawns, tracks, and messages agents. Holds `Arc<Mutex<TaskManager>>`, active instances, custom agent defs, event broadcast, and tool executor factory. `spawn_named_agent()` is the production entry point for a configured custom agent; `spawn_agent()` remains the built-in/`AgentType` entry point.
- **`AgentInstance`** — Running agent handle with `task_handle`, `status`, `context`, `message_tx`, `output_file`, `model`, `prompt`, `completion_notify`.
- **`AgentStatus`** — Lifecycle states: `Starting`, `Running`, `Completed`, `Failed`, `Stopped`.
- **`AgentResult`** — Completion result: `output`, `status`, `token_usage`.
- **`AgentError`** — Error type: `NotFound`, `NotRunning`, `SpawnFailed`, `ChannelClosed`, `ValidationFailed`, `Timeout`, `UnknownCustomAgent { name, available }`. The unknown-agent error lists the configured names (or says that none are configured and where they go).

#### Spawn Configuration

- **`SpawnOptions`** — Full spawn configuration:
  - `run_in_background` — non-blocking mode
  - `model_override` — model selection
  - `allowed_tools` — tool whitelist
  - `isolation` — worktree isolation mode
  - `cwd` — working directory
  - `extra_env` — **refused when non-empty.** Sub-agents run in-process, so there is no
    child environment to inject; the only substitute (`std::env::set_var`) would leak into
    the parent. The spawn returns a hard configuration error naming the keys.
  - `writable` — false enforces read-only tool enforcement
  - `max_tokens` / `max_duration_seconds` / `max_turns` — resource limits
  - `output_schema` — JSON schema for output validation
  - `retry_policy` — retry on failure
  - `sandbox` — sandbox config override
  - `parent_prefix` — parent conversation messages the fork reuses verbatim (empty = fresh prefix)
  - `parent_cache_safe_params` — `CacheSafeParams` that must match the parent for the prefix to be cache-safe

- **`RetryPolicy`** — Retry configuration: `max_retries`, `on_failure` (`Retry` or `EscalateToParent`), `escalation_prompt`.
- **`FailureAction`** — What to do when retries exhausted.

#### Context & Messaging

- **`ForkedContext`** — Context inherited from parent: `parent_session_id`, `cwd`, `recent_message_count`, `allowed_tools`, `env_snapshot`, plus the cache-aware fork fields `conversation_prefix`, `fork_point` and `cache_safe_params`. Builder pattern with `with_tools()`, `with_env()`, `with_conversation_prefix()` and `with_cache_safe_params()`.
- **`CacheSafeParams`** — The request components that must match the parent for a fork to hit the provider's prefix cache: `model`, `system_prompt`, `tool_names`.
- **`ParentContextSnapshot`** — Serializable snapshot of the parent at turn start: `messages`, `model`, `system_prompt`, `tool_names`. `cache_safe_params()` derives the matching [`CacheSafeParams`], and `has_prefix()` reports whether there is a prefix worth forking. The REPL refreshes one before each turn and the Agent tool reads it at spawn time.
- **`AgentMessage`** — Parent-to-agent message: `agent_id`, `content`, `message_id`.

#### Events & Observability

- **`AgentObservableEvent`** — Broadcastable event: `agent_id` + `EngineEvent`.
- **`AgentSubscription`** — Filtered receiver over the manager-wide broadcast. `recv()`/`try_recv()` drop events for other agents.
- **`TeammateEvent`** — Agent lifecycle events: `Idle { agent_id, reason }`, `TaskCompleted { agent_id, subject, summary }`.
- **`TeammateEventCallback`** — Callback type for teammate events. The REPL installs one at session startup; it forwards each event as `EngineEvent::AgentEvent` (Completed/Idle) and logs it at info level, so the UI and headless logs share the existing event path.

#### Helper Functions

- **`run_forked_agent()`** — Delegation helper on the core orchestrator. It is implemented and tested but has no production caller; the product path for delegation is the `Agent` tool.
- **`run_background_feature()`** — Background AI feature execution through the core orchestrator.

`execute_via_agent()` and `spawn_dream_agent()` were removed as dead APIs: nothing called them, and the background/dream paths go through `run_background_feature`.

## Architecture Patterns

- **Real `QueryEngine` execution** — Spawned agents run a full `QueryEngine` from `coderex-core` with tool whitelisting and read-only enforcement.
- **Channel-based messaging** — Parent and child agents communicate via `mpsc::UnboundedChannel`; messages are drained and appended to the prompt before each engine submission.
- **Context forking** — Parent snapshots its state (session ID, cwd, tool list, env vars) for the child to work from.
- **Cache-aware forking** — When the caller supplies `parent_prefix`, the fork reuses the parent's conversation messages *verbatim* (cloned, not re-derived, so the bytes match what the parent sent) and appends its own task after `fork_point`. `prefix_fingerprint()` gives a stable hash of the prefix for drift detection, and `is_cache_safe()` reports whether a given model/system-prompt/tool set would still hit the parent's prefix cache. A fork with no prefix starts a fresh one. CodeRex messages carry no stable uuid, so the fork point is an index, not an id.
- **Tool executor factory** — Since `coderex-agents` cannot depend on `coderex-tools`, the caller provides a factory closure that creates concrete `ToolExecutor` implementations.
- **Retry with escalation** — Failed agents retry up to `max_retries` times with error context and optional escalation prompt appended to the prompt.
- **Schema validation** — Agent outputs validated against JSON schema via `StructuredOutputValidator` with automatic retry on validation failure.
- **Observability pipeline** — Agent events flow to three destinations simultaneously:
  1. Broadcast channel (`subscribe_to_agent()`) for real-time streaming
  2. NDJSON log files at `~/.coderex/agent_logs/{agent_id}.jsonl`
  3. Parent engine event bus (`EngineEvent::AgentEvent`)
- **Sandbox override** — Per-agent `SandboxConfig` injected into the tool executor factory.
- **Token tracking** — `QueryEngine.total_usage()` captured after each attempt, persisted to companion `.meta.json` file alongside the output file.
- **Custom agent definitions** — Users define agents in `~/.coderex/agents/{name}.json`; `spawn_named_agent(name, ...)` resolves the definition and delegates to `AgentType::Custom`, which merges the system prompt, tool whitelist, model, token limit and output schema into the spawn.
- **Named agents from the CLI** — `coderex --agent <name>` resolves the same definitions at session start. The definition's system prompt replaces the default session prompt, its `allowed_tools` narrows the session, and its `model` / `max_tokens` are installed on the session `QueryEngine`. An explicit `--system-prompt`, `--allowed-tools` or `--model` wins (there is no CLI max-token flag, so the definition's limit always applies), and an unknown name fails before the REPL starts with the list of configured names. The `coderex agents` subcommand lists built-in and custom agents.
- **Parent prefix wiring** — The REPL refreshes a [`ParentContextSnapshot`] immediately before each turn and the Agent tool's parent-context provider reads it when spawning. When the snapshot has messages, the tool sets `SpawnOptions::parent_prefix` and `parent_cache_safe_params` (derived from the same model/system-prompt/tool list) and reports `message_count = prefix.len()`, so `ForkedContext` knows where the parent prefix ends. Without a provider (managers used directly, tests) a spawn still works but starts a fresh prefix.
- **Lifecycle producers** — `run_agent_task()` emits `AgentLifecycleEvent::Started`, `TextDelta` for each streamed text delta, `ToolCall { name, args }` for each tool invocation (the engine exposes the call summary, so `args` currently carries `{"summary": ...}` rather than the parsed arguments), `Idle { reason: "turn complete" }` after the event stream drains, and finally `Completed` or `Failed`. They travel as `EngineEvent::AgentEvent`, which the TUI already renders.
- **External subscriptions** — `subscribe_to_agent(id)` returns an `AgentSubscription`, a wrapper over the manager-wide `tokio::sync::broadcast` channel that yields only events tagged with that agent id; `subscribe_all()` (and the `"all"` alias) keeps the unfiltered view. The REPL creates an unfiltered subscription in `with_engine` and drains it on the TUI tick and in headless event draining (debug logs); the UI itself uses `EngineEvent::AgentEvent`, so the broadcast is the external-observer surface, not a second rendering path. `register_agent()` was removed: there is no transport for an externally managed agent in CodeRex (sub-agents are in-process `tokio` tasks), so it could only ever record a dangling id.

## Permission inheritance

A sub-agent inherits the parent's **permission mode only**. The child builds a fresh
`PermissionChecker` with that mode; the parent's rules, hooks, and rule exemptions are
not carried over. The background-denial hook is added to every child, and only a child
spawned with `run_in_background` carries an `agent_id` (so the hook is a no-op for
foreground agents).

## Current Status

All wired features are implemented. No known stubs. Two documented boundaries remain:
`run_forked_agent` has no production caller, and `extra_env` is rejected rather than
silently ignored.

### What Works

- **Agent type definitions** — All 7 built-in types plus custom agents.
- **Custom agent loading** — Scans `~/.coderex/agents/*.json` on manager creation; merged options override spawn defaults.
- **AgentManager** — Full lifecycle: spawn (built-in or named custom), send_message, complete, stop, list, running_count.
- **Real engine execution** — `spawn_agent()` creates a `tokio::spawn` task running `run_agent_task()` with a full `QueryEngine`.
- **Context forking** — `ForkedContext` with session ID, cwd, message count, tool whitelist, environment snapshot, and — for cache-aware forks — the parent conversation prefix, fork point and cache-safe params.
- **Messaging** — `send_message()` via `mpsc::UnboundedChannel`; messages drained and appended to prompt before each engine submission.
- **Resource limits** — `max_tokens`, `max_duration_seconds`, `max_turns` enforced per attempt.
- **Retry policy** — Configurable retries with escalation prompt and error context.
- **Output validation** — JSON schema validation via `StructuredOutputValidator` with automatic retry.
- **Read-only enforcement** — `writable: false` filters tool whitelist to read-only tools only.
- **Sandbox integration** — Per-agent `SandboxConfig` override passed through tool executor factory.
- **Token usage tracking** — Captured from `QueryEngine.total_usage()`, persisted to `.meta.json`, returned by `wait_for_completion()`.
- **Observability** — Broadcast channel, NDJSON log files, parent event bus integration.
- **Teammate events** — `TaskCompleted` and `Idle` events emitted through the manager callback. The REPL is a production caller: it forwards them to `EngineEvent::AgentEvent` and logs each one, so they reach the UI and headless logs.
- **Sub-agent lifecycle events** — `Started`, `TextDelta`, `ToolCall`, `Idle` and terminal `Completed`/`Failed` are emitted to the parent engine (see the lifecycle pattern above).
- **Cache-aware forking** — `ForkedContext` reuses the parent's conversation prefix verbatim (`fork_messages()`, `prefix_fingerprint()`, `is_cache_safe()`), so a fork can hit the provider's prefix cache instead of paying for a fresh prefix. The REPL supplies the production prefix/cache-safe params through `ParentContextSnapshot`.
- **Forked/background delegation** — `run_forked_agent()` and `run_background_feature()` via `coderex-core` orchestrator.
- **Custom agent resolution** — `spawn_named_agent()` resolves named custom agents from loaded definitions, merging allowed_tools, model, max_tokens, and output_schema. `--agent` resolves the same definition for the session and applies its model and max_tokens to the session engine, unless `--model` was explicit.
- **Agent tool spawn options** — The Agent tool exposes `agent` (custom name, takes precedence over `subagent_type`), `writable` / `read_only`, and `retry_policy` / `max_retries`, and passes them into `SpawnOptions`. `writable: false` enforces the Read/Grep/Glob whitelist.
- **Removed registration API** — `register_agent()` and `is_registered()` no longer exist: there is no external-agent transport, so they were dangling. See the pattern note above.

## Background Job Records and Adoption (`coderex-core::job_record`)

Background agents are **in-process `tokio` tasks**, and background shell
commands are child processes owned by this process. When the process exits, the
work is gone. There is no worker process outliving its supervisor, so there is
nothing to re-attach: adoption here does not fake re-attachment. What it does
is make the *record* of that work durable, and make sure the next session
reports what it finds.

**Who writes records.** A record is written at spawn and settled when the work
ends:

| Producer | Written when | Settled |
|---|---|---|
| `AgentManager::spawn_agent` (`run_in_background: true`) | before the tokio task starts | `Done`/`Failed` when the task ends, `Interrupted` on `stop_agent` or manager drop |
| `Bash` tool with `run_in_background: true` | after the child is spawned, before the tool returns | `Done`/`Failed` (with the exit code or timeout error) when the child exits |
| `CronScheduler` (`durable` definitions only) | on `create` and when `load` re-schedules a saved definition | `Done` on one-shot fire or delete; recurring fires refresh the record |

Synchronous agents and transient (non-durable) cron definitions write no
record: there is no background work to adopt, and a non-durable definition dies
with the process. Workflow runs are synchronous (`WorkflowTool` awaits each
agent), so they have no producer.

A record must never be left claiming `Running` after its work is over.
`JobStore::settle()` only transitions a record that is still `Running`, so a
late completion cannot overwrite an `Interrupted` written by cancellation or
adoption.

**The record.** `JobStore` writes one JSON file per job under
`<cwd>/.coderex/jobs/<id>.json`, atomically (a sibling temp file renamed over
the target, so concurrent readers never see a partial record). A `JobRecord`
carries `id`, `kind` (`Agent` / `Shell` / `Workflow` / `Cron`), `description`,
`cwd`, `session_id`, `pid`, `started_at_ms`, `updated_at_ms`, `state`
(`Running` / `Interrupted` / `Done` / `Failed`), and — added for the write side
with `serde(default)` so older records still decode — `ended_at_ms`,
`exit_code` (real processes only) and `error` (failure detail).

**No roster index.** The reference keeps a `roster.json` index beside its
per-job `state.json` files because its daemon rescans a job tree of unknown
size on every restart. CodeRex's job directory is written by one session and
holds a handful of files, so `JobStore::list()` just reads the directory. A
second copy of the same facts is a second thing that can be wrong.

**The classifier.** `adopt(records, now_ms, session_id) -> AdoptOutcome` is a
pure function — no IO, no clock, no environment. Every record lands in exactly
one bucket:

| Bucket | Rule |
|---|---|
| `foreign` | `session_id` is set and differs from the adopting session |
| `terminal` | state is `Done`/`Failed` — checked **before** ageing, so a cleanly-exited job is never "stale" |
| `stale` | last update more than `STALE_THRESHOLD_MS` (120 s) old, **or** more than `CLOCK_SKEW_ALLOWANCE_MS` (60 s) ahead of local time |
| `resumable` | the kind is genuinely re-attachable — currently `Cron` only |
| `interrupted` | everything else: work that was alive when written and cannot be re-attached |

The clock-skew bound is deliberately **asymmetric**: it only ever relaxes the
future direction (a writer whose clock ran ahead, or an NTP step), it never
makes an old record fresh.

**Interrupted work is reported, not dropped.** `JobStore::mark_interrupted()`
rewrites every `interrupted` record still claiming `Running` to `Interrupted`
and returns one `TaskNotification` per transition, so a crashed background
agent is surfaced into the conversation once — never left as a phantom
`running`, and never re-reported on every subsequent start.

**Cron is already durable.** `CronScheduler` (`crates/tools`) persists
`durable` definitions to `~/.coderex/scheduled_tasks.json` and reloads them at
session start (`repl.rs` calls `load()` then `spawn_runner()`), which is what
the reference's cron adoption does — dedupe against the session store and
re-schedule. When the scheduler is constructed with a cwd and session id
(`with_job_records`), durable definitions are mirrored into the job store:
`load()` refreshes each record to `Running` (re-scheduling is a new life for
it), `create()` writes one, a recurring fire touches it, and a one-shot fire or
a `delete()` settles it `Done`. Adoption therefore classifies cron records as
`resumable` for disclosure but does not re-register them, and there is no
second cron store.

**Kill switch.** `CODEREX_DISABLE_ADOPT` (non-empty, not `0`/`false`/`no`)
makes `adopt_for_session()` a no-op. As in the reference, there is **no
adopt-time consent prompt** — consent is carried, and adoption *discloses*
rather than asks — so this is a switch, not a dialog.

**Session entry point.** One call, from the session setup in `repl.rs` (next to
the existing `TaskStore` / `CronScheduler` wiring):

```rust
let store = coderex_core::job_record::JobStore::new(&cwd);
let adopted = coderex_core::job_record::adopt_for_session(
    &store,
    coderex_core::task::now_ms(),
    &session_id,
)
.unwrap_or_default();
for n in &adopted.notifications {
    ui_state.messages.push(Message {
        role: MessageRole::System,
        content: n.text.clone().into(),
        tool_name: None,
        streaming: false,
        folded: false,
    });
}
```

## API Quick Reference

### Spawning an Agent

```rust
let mut mgr = AgentManager::standalone();
mgr.set_api_client(api_client);
mgr.set_tool_executor_factory(|cwd, sandbox| {
    let mut dispatcher = create_dispatcher(cwd);
    dispatcher.set_sandbox_override(sandbox);
    Box::new(dispatcher)
});

let options = SpawnOptions {
    run_in_background: false,
    allowed_tools: Some(vec!["Read".into(), "Grep".into()]),
    max_tokens: Some(2048),
    max_duration_seconds: Some(300),
    max_turns: Some(20),
    sandbox: Some(SandboxConfig { /* ... */ }),
    retry_policy: Some(RetryPolicy::default()),
    output_schema: Some(schema),
    ..Default::default()
};

let agent = mgr
    .spawn_agent(
        AgentType::Explore { thoroughness: Thoroughness::Medium },
        "Find all TODO comments",
        options,
        "parent_session",
        "/project",
        10,
    )
    .await?;
```

### Waiting for Completion

```rust
let result = mgr.wait_for_completion(&agent.task_handle.id).await?;
println!("Status: {:?}", result.status);
println!("Output: {}", result.output);
println!("Tokens used: {:?}", result.token_usage);
```

### Sending Messages to a Running Agent

```rust
mgr.send_message(&agent.task_handle.id, "Please also search for FIXME comments").await?;
```

Messages are drained and appended to the agent's prompt context before each engine submission.

### Subscribing to Agent Events

```rust
let mut rx = mgr.subscribe_to_agent(&agent_id).unwrap();
while let Ok(event) = rx.recv().await {
    // event: AgentObservableEvent { agent_id, event: EngineEvent }
    // Only events for `agent_id` are returned; other agents are filtered out.
}

// Or observe every agent.
let mut all = mgr.subscribe_all().unwrap();
```

The manager keeps one broadcast channel for all agents. `subscribe_to_agent`
wraps that receiver in an [`AgentSubscription`] that drops events belonging to
other agents; `"all"` and `subscribe_all()` retain the manager-wide view. A
receiver is present only when the manager was created with an event broadcast.
The REPL is the production subscriber; the TUI reads the same lifecycle
through `EngineEvent::AgentEvent`.

### Custom Agent Config (`~/.coderex/agents/my_agent.json`)

```json
{
  "name": "my_agent",
  "system_prompt": "You are a security review agent.",
  "allowed_tools": ["Read", "Grep", "Glob"],
  "model": "claude-opus",
  "max_tokens": 4096,
  "output_schema": null
}
```

Spawn by name (this is what the Agent tool and `--agent` use):

```rust
let agent = mgr
    .spawn_named_agent(
        "my_agent",
        "Review auth.rs for vulnerabilities",
        SpawnOptions::default(),
        "parent_session",
        "/project",
        0,
    )
    .await?;
```

An unknown name returns `AgentError::UnknownCustomAgent`, whose message lists
the configured names (or explains where to add a definition when none are
configured) instead of silently falling back to a general-purpose agent.

When a definition is used for the **session** (`coderex --agent <name>`), its
`system_prompt`, `allowed_tools`, `model` and `max_tokens` are applied to the
session (the model and token limit via `QueryEngine::set_model` /
`set_max_tokens`). Explicit `--system-prompt` or `--allowed-tools` win, and an
explicit `--model` wins over the file. There is no CLI max-token flag, so the
definition's `max_tokens` always applies.

### Parent Prefix / Cache-Safe Spawn

```rust
let snapshot = ParentContextSnapshot::new(messages, model, system_prompt, tool_names);
let options = SpawnOptions {
    parent_prefix: snapshot.messages.clone(),
    parent_cache_safe_params: Some(snapshot.cache_safe_params()),
    ..Default::default()
};
let agent = mgr
    .spawn_agent(
        AgentType::GeneralPurpose,
        "Summarize the change",
        options,
        "parent_session",
        "/project",
        snapshot.messages.len(),
    )
    .await?;
```

`ParentContextSnapshot::cache_safe_params()` derives `model`,
`system_prompt` and `tool_names` from the same snapshot, so the prefix and
the params cannot disagree. The REPL builds this snapshot before each turn and
the Agent tool reads it through `with_parent_context_provider()`.
