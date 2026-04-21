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
│  │   engine events ──► broadcast<TelemetryEvent>            │  │  │
│  │                   ──► NDJSON log (~/.coderex/agent_logs) │  │  │
│  │                   ──► parent EngineEvent channel         │  │  │
│  └────────────────────────────────────────────────────────────┘  │
│                                                                  │
│  Helper Functions:                                               │
│   • execute_via_agent()       — short-lived command execution    │
│   • spawn_dream_agent()       — read-only background analysis    │
│   • run_forked_agent()        — delegate to core orchestrator    │
│   • run_background_feature()  — background AI feature via core   │
└──────────────────────────────────────────────────────────────────┘
```

## Key Types

### Agent Types (`types.rs`)

- **`AgentType`** — Built-in agent variants: `GeneralPurpose`, `Explore { thoroughness }`, `Plan`, `Verification`, `ClaudeCodeGuide`, `StatuslineSetup`, `Custom { prompt }`.
- **`Thoroughness`** — Search depth for Explore agents: `Quick`, `Medium`, `VeryThorough`.

### Custom Agents (`custom_agents.rs`)

- **`CustomAgentDef`** — User-defined agent loaded from `~/.coderex/agents/*.json` with fields: `name`, `system_prompt`, `allowed_tools`, `model`, `max_tokens`, `output_schema`.
- **`load_custom_agents()`** — Scans `~/.coderex/agents/*.json` and returns a name-to-definition map.
- **`get_custom_agent()`** — Lookup helper for a named custom agent.

### Agent Manager (`manager.rs`)

#### Core Types

- **`AgentManager`** — Central manager: spawns, tracks, messages, resumes agents. Holds `Arc<Mutex<TaskManager>>`, active instances, custom agent defs, event broadcast, and tool executor factory.
- **`AgentInstance`** — Running agent handle with `task_handle`, `status`, `context`, `message_tx`, `output_file`, `model`, `prompt`, `completion_notify`.
- **`AgentStatus`** — Lifecycle states: `Starting`, `Running`, `Completed`, `Failed`, `Stopped`.
- **`AgentResult`** — Completion result: `output`, `status`, `token_usage`.
- **`AgentError`** — Error type: `NotFound`, `NotRunning`, `SpawnFailed`, `ChannelClosed`, `ValidationFailed`, `Timeout`.

#### Spawn Configuration

- **`SpawnOptions`** — Full spawn configuration:
  - `run_in_background` — non-blocking mode
  - `model_override` — model selection
  - `allowed_tools` — tool whitelist
  - `isolation` — worktree isolation mode
  - `cwd` — working directory
  - `extra_env` — injected environment variables
  - `writable` — false enforces read-only tool enforcement
  - `max_tokens` / `max_duration_seconds` / `max_turns` — resource limits
  - `output_schema` — JSON schema for output validation
  - `retry_policy` — retry on failure
  - `sandbox` — sandbox config override

- **`RetryPolicy`** — Retry configuration: `max_retries`, `on_failure` (`Retry` or `EscalateToParent`), `escalation_prompt`.
- **`FailureAction`** — What to do when retries exhausted.

#### Context & Messaging

- **`ForkedContext`** — Context inherited from parent: `parent_session_id`, `cwd`, `recent_message_count`, `allowed_tools`, `env_snapshot`. Builder pattern with `with_tools()` and `with_env()`.
- **`AgentMessage`** — Parent-to-agent message: `agent_id`, `content`, `message_id`.

#### Events & Observability

- **`AgentObservableEvent`** — Broadcastable event: `agent_id` + `EngineEvent`.
- **`TeammateEvent`** — Agent lifecycle events: `Idle { agent_id, reason }`, `TaskCompleted { agent_id, subject, summary }`.
- **`TeammateEventCallback`** — Callback type for teammate events.

#### Helper Functions

- **`execute_via_agent()`** — Spawn a short-lived agent with a command as prompt, wait for completion, return output.
- **`spawn_dream_agent()`** — Spawn a read-only background agent (Read/Glob/Grep whitelist) to analyze sessions and produce summaries.

## Architecture Patterns

- **Real `QueryEngine` execution** — Spawned agents run a full `QueryEngine` from `coderex-core` with tool whitelisting and read-only enforcement.
- **Channel-based messaging** — Parent and child agents communicate via `mpsc::UnboundedChannel`; messages are drained and appended to the prompt before each engine submission.
- **Context forking** — Parent snapshots its state (session ID, cwd, tool list, env vars) for the child to work from.
- **Tool executor factory** — Since `coderex-agents` cannot depend on `coderex-tools`, the caller provides a factory closure that creates concrete `ToolExecutor` implementations.
- **Retry with escalation** — Failed agents retry up to `max_retries` times with error context and optional escalation prompt appended to the prompt.
- **Schema validation** — Agent outputs validated against JSON schema via `StructuredOutputValidator` with automatic retry on validation failure.
- **Observability pipeline** — Agent events flow to three destinations simultaneously:
  1. Broadcast channel (`subscribe_to_agent()`) for real-time streaming
  2. NDJSON log files at `~/.coderex/agent_logs/{agent_id}.jsonl`
  3. Parent engine event bus (`EngineEvent::AgentEvent`)
- **Sandbox override** — Per-agent `SandboxConfig` injected into the tool executor factory.
- **Token tracking** — `QueryEngine.total_usage()` captured after each attempt, persisted to companion `.meta.json` file alongside the output file.
- **Custom agent definitions** — Users define agents in `~/.coderex/agents/{name}.json`; resolved by name in `AgentType::Custom`.
- **Resume support** — Agents from previous sessions can be resumed with `resume_agent()`, restoring their context and state.

## Current Status

All features are implemented. No known stubs.

### What Works

- **Agent type definitions** — All 7 built-in types plus custom agents.
- **Custom agent loading** — Scans `~/.coderex/agents/*.json` on manager creation; merged options override spawn defaults.
- **AgentManager** — Full lifecycle: spawn, send_message, complete, stop, list, running_count, resume.
- **Real engine execution** — `spawn_agent()` creates a `tokio::spawn` task running `run_agent_task()` with a full `QueryEngine`.
- **`execute_via_agent()`** — Short-lived agent execution for command-style tasks.
- **`spawn_dream_agent()`** — Read-only background analysis agent with Read/Glob/Grep whitelist.
- **Context forking** — `ForkedContext` with session ID, cwd, message count, tool whitelist, and environment snapshot.
- **Messaging** — `send_message()` via `mpsc::UnboundedChannel`; messages drained and appended to prompt before each engine submission.
- **Resource limits** — `max_tokens`, `max_duration_seconds`, `max_turns` enforced per attempt.
- **Retry policy** — Configurable retries with escalation prompt and error context.
- **Output validation** — JSON schema validation via `StructuredOutputValidator` with automatic retry.
- **Read-only enforcement** — `writable: false` filters tool whitelist to read-only tools only.
- **Sandbox integration** — Per-agent `SandboxConfig` override passed through tool executor factory.
- **Token usage tracking** — Captured from `QueryEngine.total_usage()`, persisted to `.meta.json`, returned by `wait_for_completion()`.
- **Observability** — Broadcast channel, NDJSON log files, parent event bus integration.
- **Teammate events** — `TaskCompleted` and `Idle` events emitted with callback support.
- **Resume support** — `resume_agent()` restores agents from previous sessions.
- **Forked/background delegation** — `run_forked_agent()` and `run_background_feature()` via `coderex-core` orchestrator.
- **Custom agent resolution** — `AgentType::Custom` resolves named custom agents from loaded definitions, merging allowed_tools, model, max_tokens, and output_schema.

## API Quick Reference

### Spawning an Agent

```rust
let mut mgr = AgentManager::standalone();
mgr.set_api_client(api_client);
mgr.set_tool_executor_factory(|sandbox| {
    let mut dispatcher = create_dispatcher();
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
}
```

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

Spawn by name:

```rust
let agent = mgr
    .spawn_agent(
        AgentType::Custom { prompt: "my_agent".into() },
        "Review auth.rs for vulnerabilities",
        SpawnOptions::default(),
        "parent_session",
        "/project",
        0,
    )
    .await?;
```
