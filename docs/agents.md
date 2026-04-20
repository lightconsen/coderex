# agents Module Inspection

## Goal

Sub-agent system for spawning autonomous worker agents, dream agents (background summarization), and forked contexts. Enables parallel task execution and delegated work items.

## Design

### Key Types

- **`AgentManager`** (`manager.rs`) — Spawns, tracks, and communicates with sub-agents.
- **`AgentInstance`** (`manager.rs`) — Running agent handle with channels, completion notification, and output file path.
- **`AgentType`** (`types.rs`) — `GeneralPurpose`, `Explore`, `Plan`, `Verification`, `ClaudeCodeGuide`, `StatuslineSetup`, `Custom`.
- **`Thoroughness`** (`types.rs`) — `Quick`, `Medium`, `VeryThorough` — controls agent depth for `Explore` agents.
- **`SpawnOptions`** (`manager.rs`) — Configuration for new agents including tool whitelisting, model override, resource limits, sandbox, retry policy, and output schema.
- **`ForkedContext`** (`manager.rs`) — Snapshot of parent context for agent delegation.
- **`AgentMessage`** (`manager.rs`) — Communication protocol between parent and agents.
- **`TeammateEvent`** (`manager.rs`) — Agent lifecycle events.
- **`AgentLifecycleEvent`** (`core/src/types.rs`) — `Started`, `Completed`, `Failed`, `Idle`, `ToolCall`, `TextDelta`.
- **`AgentObservableEvent`** (`manager.rs`) — Broadcastable event wrapper with `agent_id` and `event`.
- **`CustomAgentDef`** (`custom_agents.rs`) — User-defined agent loaded from `~/.coderex/agents/*.json`.

### Architecture Patterns

- **Channel-based communication** — Parent and child agents communicate via mpsc.
- **Context forking** — Parent snapshots its state for the child to work from.
- **Thoroughness levels** — Agents adjust their approach based on requested depth.
- **Real engine execution** — Spawned agents run a full `QueryEngine` with tool whitelisting and read-only enforcement.
- **Event streaming** — Agent events flow to broadcast subscribers, NDJSON log files, and the parent engine event bus.
- **Sandbox override** — Per-agent `SandboxConfig` can be injected into the tool executor.
- **Retry with context** — Failed agents retry with error context appended to the prompt.
- **Schema validation** — Agent outputs can be validated against a JSON schema with automatic retry.
- **Custom agent types** — Users can define agents in `~/.coderex/agents/{name}.json`.

## Current Status

### What Works

- **Agent type and thoroughness definitions** (`types.rs`).
- **Spawn options and context forking types** (`manager.rs`).
- **Agent manager** with real `QueryEngine` spawning via `tokio::spawn`.
- **`execute_via_agent()`** — Spawns a short-lived agent, waits for completion, returns output.
- **`spawn_dream_agent()`** — Spawns a read-only background agent with `Read`/`Glob`/`Grep` whitelist.
- **Agent messaging** — `send_message()` sends messages via mpsc; `run_agent_task` drains the receiver and appends messages to the engine prompt before each attempt and during event collection.
- **Token usage tracking** — `AgentResult.token_usage` is populated from `QueryEngine.total_usage()`, persisted to a companion `.meta.json` file, and returned by `wait_for_completion()`.
- **Teammate events** — `TaskCompleted`, `TaskFailed`, `Idle` events emitted and forwarded to `EngineEvent::AgentEvent`.
- **Agent observability**:
  - Broadcast channel (`subscribe_to_agent()`) for real-time event streaming.
  - NDJSON log files written to `~/.coderex/agent_logs/{agent_id}.jsonl`.
  - Parent event bus integration via `event_tx`.
- **Resource limits** — `max_tokens`, `max_duration_seconds`, `max_turns` enforced by the engine.
- **Retry/fallback policy** — `RetryPolicy` with `max_retries`, `on_failure` action, and `escalation_prompt`.
- **Output validation** — `output_schema` on `SpawnOptions` validated with `StructuredOutputValidator`.
- **Sandbox integration** — Per-agent `SandboxConfig` override passed through the tool executor factory.
- **Custom agent definitions** — Loaded from `~/.coderex/agents/*.json`; resolved by name in `AgentType::Custom`.

### What Is Stubbed/Partial

No known stubbed items. All documented features are implemented.

## API Quick Reference

### Spawning an Agent

```rust
let mut mgr = AgentManager::standalone();
mgr.set_api_client(api_client);
mgr.set_tool_executor_factory(|sandbox| {
    // Create a RegistryToolDispatcher with optional sandbox override
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

Messages are appended to the agent's prompt context and will be picked up before the next engine submission or during event collection.

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
