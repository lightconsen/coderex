# coderex vs cc-src vs kimi-cli vs qwen-code vs gemini-cli

A full module-by-module comparison of `coderex` against four reference code agents:
**cc-src** (Claude Code TS source), **kimi-cli** (Moonshot Python), **qwen-code** (Alibaba TS),
**gemini-cli** (Google TS). Includes per-module best-design picks, references coderex should
borrow, and a dedicated section on how each project does testing (unit, integration, e2e, evals).

## 0. Project snapshot

```
┌────────────┬──────────────┬──────────────────────────────────────┬────────────────────────────────────────────────────┬────────────────────────────────┬──────────────────────────────────────────────────────────────────┐
│ Project    │ Lang         │ Size                                 │ Shape                                              │ License                        │ Distinctive identity                                             │
├────────────┼──────────────┼──────────────────────────────────────┼────────────────────────────────────────────────────┼────────────────────────────────┼──────────────────────────────────────────────────────────────────┤
│ coderex    │ Rust         │ ~47k LOC, 14 crates                  │ Cargo workspace                                    │ —                              │ Typed, team/swarm + bridge remote-control, spec-driven           │
├────────────┼──────────────┼──────────────────────────────────────┼────────────────────────────────────────────────────┼────────────────────────────────┼──────────────────────────────────────────────────────────────────┤
│ cc-src     │ TS (bundled) │ 513k LOC, 1.8k files                 │ Flat monolith, ~42 tool dirs, 101 slash cmds       │ Proprietary (leaked/reference) │ The reference — feature-maximal                                  │
├────────────┼──────────────┼──────────────────────────────────────┼────────────────────────────────────────────────────┼────────────────────────────────┼──────────────────────────────────────────────────────────────────┤
│ kimi-cli   │ Python       │ 59k LOC, 966 files                   │ pyproject + Nix flake, pytest                      │ Apache-2.0                     │ Wire-protocol first (ACP), snapshot e2e, VS Code + Web UI        │
├────────────┼──────────────┼──────────────────────────────────────┼────────────────────────────────────────────────────┼────────────────────────────────┼──────────────────────────────────────────────────────────────────┤
│ qwen-code  │ TS           │ 470k LOC, pnpm monorepo (9 pkgs)     │ cli/core/sdk-ts/sdk-java/vscode/zed/webui/channels │ Apache-2.0                     │ Hardened hooks, multi-provider channels, cross-IDE               │
├────────────┼──────────────┼──────────────────────────────────────┼────────────────────────────────────────────────────┼────────────────────────────────┼──────────────────────────────────────────────────────────────────┤
│ gemini-cli │ TS           │ 570k LOC, 6 packages, 828 test files │ cli/core/a2a-server/sdk/devtools/vscode            │ Apache-2.0                     │ A2A agent mesh, deep eval harness, sandbox (3 OS), policy engine │
└────────────┴──────────────┴──────────────────────────────────────┴────────────────────────────────────────────────────┴────────────────────────────────┴──────────────────────────────────────────────────────────────────┘
```

---

## 1. Module-by-module comparison

Legend:  ★ = best reference design  •  ⚠ = notable gap in coderex

### 1.1 Query engine / turn loop

```
┌───────────────────────────────────────────────────────┬───────────────────────────────────────────────────┬─────────────────────────────────────┬────────────────────────────────┬──────────────────────────────────────────────────────────────────────┐
│ coderex                                               │ cc-src                                            │ kimi-cli                            │ qwen-code                      │ gemini-cli                                                           │
├───────────────────────────────────────────────────────┼───────────────────────────────────────────────────┼─────────────────────────────────────┼────────────────────────────────┼──────────────────────────────────────────────────────────────────────┤
│ core::QueryEngine + TurnState; compact +              │ Reference design for turn loop, compaction,       │ soul/agent.py + runtime; smaller    │ packages/core/core/ with       │ core/core/ + scheduler/ with state-manager, parallel tool exec,      │
│ PromptCompiler per spec §5.11                         │ tool-call streaming ★                             │ loop, session_fork                  │ provider routing               │ waiting-callback ★                                                   │
└───────────────────────────────────────────────────────┴───────────────────────────────────────────────────┴─────────────────────────────────────┴────────────────────────────────┴──────────────────────────────────────────────────────────────────────┘
```

**Best design:** gemini-cli's `scheduler/` cleanly splits turn state, parallel tool execution, hooks, and waiting callbacks across 12 files (state-manager, tool-executor, scheduler_parallel, scheduler_waiting_callback).
**Recommend for coderex:** adopt gemini-cli's `state-manager` + `scheduler_parallel` pattern to formalize parallel-tool dispatch inside `TurnState`. Adopt cc-src's compaction strategy as a reference (already done).

### 1.2 Tool registry + baseline tools

```
┌───────────────────────────────────────────────────────────────────────┬──────────────────────┬──────────────────────────┬───────────┬───────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ coderex                                                               │ cc-src               │ kimi-cli                 │ qwen-code │ gemini-cli                                                                                                        │
├───────────────────────────────────────────────────────────────────────┼──────────────────────┼──────────────────────────┼───────────┼───────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ 43 baseline + 13 feature-gated, ToolTier enum, Handle-injection       │ ~43 tools, no        │ 13 tools, narrower       │ ~27 tools │ ~40 tools incl. activate-skill, enter/exit-plan-mode, trackerTools, topicTool, jit-context, modifiable-tool ★     │
│ pattern (avoids cycles) ★                                             │ tiering              │ surface                  │           │                                                                                                                   │
└───────────────────────────────────────────────────────────────────────┴──────────────────────┴──────────────────────────┴───────────┴───────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

**Best design:** **coderex** for typed tiering (`ToolTier::{Baseline,FeatureGated,DynamicMcp,Internal}`) — no other agent has this. **gemini-cli** for runtime mechanics (`tool-registry.ts`, `modifiable-tool.ts`, `omissionPlaceholderDetector`, `diff-utils` as first-class modules).
**Recommend for coderex:** borrow gemini's `jit-context` (just-in-time context expansion) and `modifiable-tool` (tool can rewrite its own args before execution). Borrow gemini's `omissionPlaceholderDetector` to detect `…` ellipsis in model-generated edits.

### 1.3 Hooks / lifecycle events

```
┌─────────────────────────────────────────────────────┬──────────────────────────┬─────────────────────────────────┬──────────────────────────────────────────────────────────────────┬───────────────────────────────────────────────────────────┐
│ coderex                                             │ cc-src                   │ kimi-cli                        │ qwen-code                                                        │ gemini-cli                                                │
├─────────────────────────────────────────────────────┼──────────────────────────┼─────────────────────────────────┼──────────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────────────┤
│ hooks::engine + StopHook, 20 events, basic exec ⚠   │ React-style hooks +      │ soul/dynamic_injection.py,      │ 6-layer: Registry→Planner→Runner→Aggregator→EventHandler→System  │ 7 modules incl. trustedHooks (integrity), runtimeHooks,   │
│                                                     │ permission hooks         │ ~10 events                      │ + ssrfGuard + envInterpolator + httpHookRunner ★                 │ hookTranslator, hookPlanner                               │
└─────────────────────────────────────────────────────┴──────────────────────────┴─────────────────────────────────┴──────────────────────────────────────────────────────────────────┴───────────────────────────────────────────────────────────┘
```

**Best design:** **qwen-code** — most hardened HTTP-hook stack (SSRF guard, URL validator, env-var interpolation, HTTP hook runner). **gemini-cli** is close second with `trustedHooks` (integrity-checked hook configs).
**Recommend for coderex:** port qwen-code's `ssrfGuard.ts` + `urlValidator.ts` + `envInterpolator.ts` to coderex's `hooks` crate. Port gemini's `trustedHooks` integrity-check pattern when wiring HTTP hooks.

### 1.4 Agents / sub-agents / delegation

```
┌──────────────────────────────────────────────┬─────────────────────────────────────────────┬──────────────────────────────────┬─────────────────────────────────┬────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ coderex                                      │ cc-src                                      │ kimi-cli                         │ qwen-code                       │ gemini-cli                                                                                                             │
├──────────────────────────────────────────────┼─────────────────────────────────────────────┼──────────────────────────────────┼─────────────────────────────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ agents::manager + swarm/team types ⚠         │ AgentTool + TaskCreate + background tasks   │ tools/agent/, single sub-agent   │ tools/agent/ + specialist agent │ generalist + codebase-investigator + cli-help + skill-extraction + memory-manager + browser; agent-scheduler; A2A ★    │
│                                              │                                             │                                  │                                 │ server (separate package); registry + remote-invocation                                                                 │
└──────────────────────────────────────────────┴─────────────────────────────────────────────┴──────────────────────────────────┴─────────────────────────────────┴────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

**Best design:** **gemini-cli** — richest agent taxonomy (6 specialised + generalist), dedicated `agent-scheduler.ts`, plus a separate `a2a-server` package for inter-agent communication (A2A protocol).
**Recommend for coderex:** add specialised agents (codebase-investigator, memory-manager, skill-extraction) to `agents` crate as first-class types beyond the generic swarm/team. Consider an `a2a-server` companion crate for cross-process agent mesh.

### 1.5 Memory

```
┌─────────────────────────────────────────────────────────────────────┬───────────────────────────────────────┬─────────────────────────────────┬──────────────────────────────┬───────────────────────────────────────────────────────────┐
│ coderex                                                             │ cc-src                                │ kimi-cli                        │ qwen-code                    │ gemini-cli                                                │
├─────────────────────────────────────────────────────────────────────┼───────────────────────────────────────┼─────────────────────────────────┼──────────────────────────────┼───────────────────────────────────────────────────────────┤
│ session_memory + long_term + team_memory + auto_dream + transcript  │ session store + long-term memory      │ conversation only, no LT memory │ session store, no LT memory  │ memoryContextManager + hierarchical snippets/topics +     │
│ + session_index ★                                                   │                                       │                                 │                              │ memory-manager-agent + chatCompressionService             │
└─────────────────────────────────────────────────────────────────────┴───────────────────────────────────────┴─────────────────────────────────┴──────────────────────────────┴───────────────────────────────────────────────────────────┘
```

**Best design:** **coderex** for breadth (only one with team memory + auto-dream). **gemini-cli** for structured retrieval (hierarchical snippets/topics + dedicated memory-manager-agent).
**Recommend for coderex:** layer gemini's snippets/topics hierarchy on top of `long_term.rs` so memory has structured retrieval keys, not flat blobs. Add a `MemoryManagerAgent` that runs during compaction to curate.

### 1.6 MCP (Model Context Protocol)

```
┌───────────────────────────────────────────────────────────────────────────────────┬───────────────────────────────────┬────────────────────────────────────┬───────────────────────────────────────────────────────────────────────────────┬──────────────────────────────────────────────────────────────────┐
│ coderex                                                                           │ cc-src                            │ kimi-cli                           │ qwen-code                                                                     │ gemini-cli                                                       │
├───────────────────────────────────────────────────────────────────────────────────┼───────────────────────────────────┼────────────────────────────────────┼───────────────────────────────────────────────────────────────────────────────┼──────────────────────────────────────────────────────────────────┤
│ mcp::manager + server + proxy + oauth + elicitation + resources + channel +       │ MCPTool + McpAuthTool +           │ acp/mcp.py, narrow surface         │ mcp-client + mcp-client-manager + Google OAuth + SA impersonation + token     │ mcp-client + mcp-client-manager + xcode-mcp-fix-transport +      │
│ ListMcpResources/ReadMcpResource tools ★ (proxy is unique)                        │ ListMcpResourcesTool              │                                    │ storage ★                                                                     │ list/read mcp resources                                          │
└───────────────────────────────────────────────────────────────────────────────────┴───────────────────────────────────┴────────────────────────────────────┴───────────────────────────────────────────────────────────────────────────────┴──────────────────────────────────────────────────────────────────┘
```

**Best design:** **qwen-code** for OAuth (Google + service-account impersonation + token storage as separate module). **coderex** uniquely has an MCP **proxy**.
**Recommend for coderex:** generalise `mcp::oauth` with a pluggable provider interface mirroring qwen-code's `oauth-provider.ts` + `sa-impersonation-provider.ts`. Add a token-storage abstraction.

### 1.7 Bridge / remote control

```
┌────────────────────────────────────────────────────────────────────────────────────┬──────────────────────────────────────────────────┬──────────┬───────────┬────────────┐
│ coderex                                                                            │ cc-src                                           │ kimi-cli │ qwen-code │ gemini-cli │
├────────────────────────────────────────────────────────────────────────────────────┼──────────────────────────────────────────────────┼──────────┼───────────┼────────────┤
│ bridge crate: archive + heartbeat + permission + poller + spawner + state +        │ RemoteTriggerTool + bridge slash commands +      │ ✗        │ ✗         │ ✗          │
│ transport ★ (cleanest decomposition)                                               │ bridge-kick.ts                                   │          │           │            │
└────────────────────────────────────────────────────────────────────────────────────┴──────────────────────────────────────────────────┴──────────┴───────────┴────────────┘
```

**Best design:** **coderex** — only Rust agent with this; 7 small modules vs cc-src's monolithic implementation. Already best-in-class.
**Recommend for coderex:** keep current architecture. Consider documenting the wire format so external clients can integrate.

### 1.8 UI / terminal / IDE

```
┌────────────────────────────────────────────────────────┬─────────────────────────────────┬──────────────────────────────────────────┬────────────────────────────────────────────────────────────────┬───────────────────────────────────────────────────────────┐
│ coderex                                                │ cc-src                          │ kimi-cli                                 │ qwen-code                                                      │ gemini-cli                                                │
├────────────────────────────────────────────────────────┼─────────────────────────────────┼──────────────────────────────────────────┼────────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────────────┤
│ Custom Rust TUI (buddy/overlay/selector/search/        │ React Ink monolith, VS Code +   │ TUI + ACP + VS Code extension + Web UI ★ │ React Ink + vscode-ide-companion + zed-extension + webui + web │ React Ink + vscode-ide-companion + devtools (debugger UI) │
│ message_list/notification_center) — no IDE ⚠ no Web ⚠  │ JetBrains integrations          │ (broadest user surface)                  │ templates ★                                                    │ + voice (response formatting)                             │
└────────────────────────────────────────────────────────┴─────────────────────────────────┴──────────────────────────────────────────┴────────────────────────────────────────────────────────────────┴───────────────────────────────────────────────────────────┘
```

**Best design:** **qwen-code** wins on breadth (4 surfaces). **kimi-cli**'s ACP wire protocol is the cleanest IDE-bridge contract.
**Recommend for coderex:** define a wire protocol (JSON over stdio or websocket) — borrow ACP shape from kimi-cli — then publish a thin VS Code extension that speaks that protocol. This unlocks IDE/Web parity without duplicating UI.

### 1.9 Config / settings / policy / sandbox

```
┌────────────────────────────────────────────────────────────────────┬──────────────────────────────────────┬───────────────────────┬───────────────────┬──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ coderex                                                            │ cc-src                               │ kimi-cli              │ qwen-code         │ gemini-cli                                                                                                           │
├────────────────────────────────────────────────────────────────────┼──────────────────────────────────────┼───────────────────────┼───────────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ settings (checksum) + org_policy + experiments + flags +           │ settings + managed settings +        │ config files,         │ config system +   │ policy/ engine (policy-engine, policy-updater, integrity, toml-loader, sandboxPolicyManager) + sandbox/ for          │
│ remote_settings + trusted_device + dirs ★ (managed-settings parity │ checksums + experiments              │ experiments           │ permission-manager│ macOS (seatbelt) + Linux (seccomp) + Windows ★                                                                       │
│ with cc-src), no sandbox ⚠                                         │                                      │                       │                   │                                                                                                                      │
└────────────────────────────────────────────────────────────────────┴──────────────────────────────────────┴───────────────────────┴───────────────────┴──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

**Best design:** **gemini-cli** — only project with OS-level sandboxing per platform plus a TOML-driven policy engine with integrity verification.
**Recommend for coderex:** add a `sandbox` crate with platform backends (`macos.rs` shelling out to `sandbox-exec`, `linux.rs` using seccomp + landlock, `windows.rs` AppContainer). Move shell + edit + write tools behind the sandbox by default.

### 1.10 Safety / permissions / confirmation

```
┌─────────────────────────────────────────────────────┬──────────────────────────────────────────────────┬──────────────────────────────────────────────┬─────────────────────────────────────────────────────────────────────┬────────────────────────────────────────────────────────────────────────┐
│ coderex                                             │ cc-src                                           │ kimi-cli                                     │ qwen-code                                                           │ gemini-cli                                                             │
├─────────────────────────────────────────────────────┼──────────────────────────────────────────────────┼──────────────────────────────────────────────┼─────────────────────────────────────────────────────────────────────┼────────────────────────────────────────────────────────────────────────┤
│ Per-tool ToolTier gating + permission via bridge ⚠  │ useCanUseTool React hook + tool permission       │ approval_runtime + soul/approval.py with     │ permission-manager + rule-parser + shell-semantics (parses bash to  │ safety/ (built-in checkers + checker-runner + registry + protocol +    │
│ no content safety, no confirmation bus              │ subsystem                                        │ approve_for_session                          │ classify intent) ★ for shell                                        │ context-builder + conseca content safety) + confirmation-bus/ ★        │
└─────────────────────────────────────────────────────┴──────────────────────────────────────────────────┴──────────────────────────────────────────────┴─────────────────────────────────────────────────────────────────────┴────────────────────────────────────────────────────────────────────────┘
```

**Best design:** **gemini-cli** for the overall stack (content safety + confirmation bus + checker pipeline). **qwen-code** for shell-specific safety (its `shell-semantics.ts` parses bash AST to detect destructive intent).
**Recommend for coderex:** introduce a `confirmation_bus` channel (decouples permission UI from tool exec — needed when adding IDE/Web UIs). Port qwen-code's `shell-semantics` into the BashTool to classify commands before exec. Add a `safety` crate with pluggable checkers.

### 1.11 Model routing / multi-provider / availability

```
┌──────────────────────────────────────┬────────────────────────────┬──────────────────────────────────┬─────────────────────────────────────────────────────────────────────────────────────────┬──────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ coderex                              │ cc-src                     │ kimi-cli                         │ qwen-code                                                                               │ gemini-cli                                                                                           │
├──────────────────────────────────────┼────────────────────────────┼──────────────────────────────────┼─────────────────────────────────────────────────────────────────────────────────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ Anthropic-flavoured api_client only ⚠│ Anthropic only             │ Moonshot only                    │ Multi-provider via channels/ package + provider plugins ★                               │ routing/ (modelRouterService + strategies/) + availability/ (modelAvailabilityService + policy +     │
│                                      │                            │                                  │                                                                                         │ errorClassification) + fallback/ (handler + types) ★                                                 │
└──────────────────────────────────────┴────────────────────────────┴──────────────────────────────────┴─────────────────────────────────────────────────────────────────────────────────────────┴──────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

**Best design:** **gemini-cli** for routing depth (strategies + availability + fallback). **qwen-code** for provider plurality (channels package).
**Recommend for coderex:** refactor `core::api_client` behind a `Provider` trait and a `ModelRouter`. Add an `availability` module that classifies provider errors and triggers `fallback` to a secondary provider. This is the largest single architectural debt vs the field.

### 1.12 Plugins / skills / extensions

```
┌─────────────────────────────────────────┬──────────────────────────────────────┬──────────────────────────────────────────────────┬──────────────────────────┬────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ coderex                                 │ cc-src                               │ kimi-cli                                         │ qwen-code                │ gemini-cli                                                                                             │
├─────────────────────────────────────────┼──────────────────────────────────────┼──────────────────────────────────────────────────┼──────────────────────────┼────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ plugins crate (plugins.rs + skills.rs)  │ SkillTool + skill catalog            │ soul/dynamic_injection.py + injections directory │ extension system in core │ skills/ (skillLoader + skillManager + builtin/) + skill-extraction-agent (agent that distils new       │
│                                         │                                      │                                                  │                          │ skills from sessions) + activate-skill tool ★                                                          │
└─────────────────────────────────────────┴──────────────────────────────────────┴──────────────────────────────────────────────────┴──────────────────────────┴────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

**Best design:** **gemini-cli** — full lifecycle: skill auto-extraction (agent) → loader → manager → activation tool.
**Recommend for coderex:** mirror `skillLoader`/`skillManager` split, add a `SkillExtractionAgent` that runs after compaction to distil reusable skills into the plugins folder.

### 1.13 SDK / programmatic API

```
┌─────────────────────────────────────────────────────┬─────────────────────────┬───────────────────────────────────────────┬───────────────────────────────────────────────────────────────┬───────────────────────────────────────────────────────────────────────┐
│ coderex                                             │ cc-src                  │ kimi-cli                                  │ qwen-code                                                     │ gemini-cli                                                            │
├─────────────────────────────────────────────────────┼─────────────────────────┼───────────────────────────────────────────┼───────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────────────────────────┤
│ sdk crate (events.rs) — Rust events only ⚠          │ JS SDK                  │ kimi-sdk separate Python package          │ sdk-typescript + sdk-java (dual language) ★                   │ sdk package: agent + session + tool + skills + fs + shell + types     │
└─────────────────────────────────────────────────────┴─────────────────────────┴───────────────────────────────────────────┴───────────────────────────────────────────────────────────────┴───────────────────────────────────────────────────────────────────────┘
```

**Best design:** **gemini-cli** for SDK API breadth (agent/session/tool/skills/fs/shell). **qwen-code** for multi-language reach (TS + Java).
**Recommend for coderex:** broaden `sdk` crate beyond events — expose `Session`, `Agent`, `Tool`, `Skill`, `Fs`, `Shell` typed APIs. Generate language bindings (Python/TS) from the Rust SDK via `pyo3` and `napi-rs`.

### 1.14 Telemetry / observability

```
┌────────────────────────────────┬────────────────────────────────┬─────────────────────────────────┬──────────────────────────┬─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ coderex                        │ cc-src                         │ kimi-cli                        │ qwen-code                │ gemini-cli                                                                                                                      │
├────────────────────────────────┼────────────────────────────────┼─────────────────────────────────┼──────────────────────────┼─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ tracing crate (cli/tracing_setup) ⚠│ telemetry hooks            │ utils/logging                   │ basic logging            │ telemetry/ (activity-detector + activity-monitor + clearcut-logger + conseca-logger + gcp-exporters + file-exporters +          │
│ no metrics, no exporters       │                                │                                 │                          │ event-loop-monitor + high-water-mark-tracker + billingEvents) ★                                                                 │
└────────────────────────────────┴────────────────────────────────┴─────────────────────────────────┴──────────────────────────┴─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

**Best design:** **gemini-cli** — full OTel-style stack with multiple exporters, activity monitoring, billing event hooks.
**Recommend for coderex:** layer `opentelemetry-rust` on top of `tracing` and add file + OTLP exporters. Add an `activity_monitor` for idle detection (powers auto-dream + UI status).

---

## 2. How each project does testing

### 2.1 Test infrastructure overview

```
┌─────────────────┬──────────────────────────────────┬──────────────────────────┬──────────────────────────────────────────┬────────────────────────────────────────────────────┬────────────────────────────────────────────────────────────────────────┐
│ Aspect          │ coderex                          │ cc-src                   │ kimi-cli                                 │ qwen-code                                          │ gemini-cli                                                             │
├─────────────────┼──────────────────────────────────┼──────────────────────────┼──────────────────────────────────────────┼────────────────────────────────────────────────────┼────────────────────────────────────────────────────────────────────────┤
│ Framework       │ cargo test (built-in)            │ unknown (no .test files) │ pytest + inline_snapshot                 │ vitest                                             │ vitest                                                                 │
│ Unit tests      │ in-crate #[cfg(test)] (1072 ✓)   │ 0 visible                │ tests/ (15+ modules)                     │ 544 .test.ts files (co-located)                    │ 828 .test.ts files (co-located)                                        │
│ Integration     │ ✗ ⚠                              │ ✗ visible                │ tests/e2e/ (in-process)                  │ integration-tests/ (14+ files)                     │ integration-tests/ (45 files)                                          │
│ E2E             │ ✗ ⚠                              │ ✗ visible                │ tests_e2e/ wire-protocol (12 tests)      │ e2e.yml + integration suite                        │ chained_e2e.yml + integration suite                                    │
│ AI evals        │ ✗ ⚠                              │ ✗ visible                │ tests_ai/ markdown scripts (manual)      │ eval.yml workflow                                  │ evals/ (30 .eval.ts) + eval-pr + evals-nightly workflows               │
│ Memory          │ ✗ ⚠                              │ ✗                        │ ✗                                        │ ✗                                                  │ memory-tests/ + baselines.json + tolerance                             │
│ Perf            │ ✗ ⚠                              │ ✗                        │ ✗                                        │ terminal-bench/ + vitest.terminal-bench.config.ts  │ perf-tests/ + baselines.json + cold-startup/long-chat/skill-loading    │
│ Test utilities  │ ✗                                │ ✗                        │ conftest.py fixtures                     │ test-helper.ts + test-mcp-server.ts                │ @google/gemini-cli-test-utils package: TestRig + harnesses             │
│ CI workflows    │ ✗ visible                        │ ✗ visible                │ 10 workflows                             │ 20 workflows                                       │ 20+ workflows incl. deflake.yml + agent-session-drift-check.yml        │
└─────────────────┴──────────────────────────────────┴──────────────────────────┴──────────────────────────────────────────┴────────────────────────────────────────────────────┴────────────────────────────────────────────────────────────────────────┘
```

### 2.2 How each agent does e2e

**gemini-cli — TestRig harness (in-process, fake-response fixtures)** 🥇
- `packages/test-utils` exports `TestRig`, `MemoryTestHarness`, `PerfTestHarness`.
- E2E tests live in `integration-tests/*.test.ts` (45 files) — examples: `acp-env-auth`, `api-resilience`, `browser-agent.{cleanup,concurrent,confirmation,…}`, `browser-policy`.
- Each test ships a sibling `.responses` directory with **deterministic recorded LLM responses** the harness replays (no real API calls in CI).
- `chained_e2e.yml` runs the suite serially, `deflake.yml` reruns flakes, `agent-session-drift-check.yml` detects behaviour drift across PRs.

**qwen-code — vitest integration suites + terminal-bench** 🥈
- `integration-tests/` covers channel-plugin, hook-integration, sdk-typescript, terminal-capture, plus `test-mcp-server.ts` for MCP scenarios.
- `vitest.terminal-bench.config.ts` runs the **terminal-bench** harness (used by industry SWE-bench-style evals) with `retry: 2`, `fileParallelism: false`, 30-min timeout.
- `concurrent-runner` directory tests parallel tool exec safety.

**kimi-cli — wire-protocol subprocess e2e** 🥉
- `tests_e2e/` spawns the agent as a subprocess and talks to it over the **ACP wire protocol** (JSON messages).
- Helpers: `start_wire`, `send_initialize`, `collect_until_response`, `summarize_messages`, `normalize_response`.
- Uses `inline_snapshot` to assert on JSON message sequences — `assert normalize_response(resp) == snapshot({…})`.
- Coverage: `test_wire_{protocol,sessions,prompt,question,errors,auth,config,approvals_tools,real_llm,mcp_cli}`.
- `tests/e2e/` adds shell-PTY tests via `shell_pty_helpers.py`.

**cc-src** — test infrastructure not in the leaked snapshot.

### 2.3 How each agent does AI evals

```
┌────────────┬──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ Project    │ Eval approach                                                                                                                                │
├────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ gemini-cli │ 30 .eval.ts files (generalist, plan_mode, save_memory, hierarchical_memory, frugalReads, frugalSearch, redundant_casts, sandbox_recovery,    │
│            │ subagents, subtask_delegation, tool_output_masking, etc.). evalTest() wraps vitest; assertions: rig.waitForToolCall(), file existence/       │
│            │ content. USUALLY_PASSES tag distinguishes flaky-but-meaningful evals. Run via eval.yml + evals-nightly.yml.                                  │
├────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ qwen-code  │ eval.yml workflow + terminal-bench (Princeton-style benchmark suite). Treats SWE-bench-class tasks as the eval signal.                      │
├────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ kimi-cli   │ tests_ai/ contains markdown test scripts (manual or LLM-judged), e.g. test_cli_loading_time.md, test_utf8_encoding.md.                      │
├────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ cc-src     │ Not visible.                                                                                                                                 │
├────────────┼──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ coderex    │ None. ⚠                                                                                                                                      │
└────────────┴──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

### 2.4 Memory and perf regression suites

Only **gemini-cli** maintains them:

- `memory-tests/memory-usage.test.ts` — `MemoryTestHarness` runs N samples, forces GC cycles, compares heap against `baselines.json` with a 10% tolerance. `UPDATE_MEMORY_BASELINES=true` to refresh.
- `perf-tests/perf-usage.test.ts` — `PerfTestHarness` measures cold startup, idle CPU, long-chat handling, Asian-language tokens, skill loading. 15% tolerance.

`qwen-code` has terminal-bench (task-completion benchmark) but no internal memory/perf baselines.

### 2.5 Recommended testing roadmap for coderex

```
┌──────┬──────────┬─────────────────────────────────────────────────────────┬─────────────────────────────────────────────────────────────────────────────────────────────────┐
│ Step │ Priority │ What to add                                             │ Reference                                                                                       │
├──────┼──────────┼─────────────────────────────────────────────────────────┼─────────────────────────────────────────────────────────────────────────────────────────────────┤
│ 1    │ High     │ Wire-protocol e2e crate (spawn coderex subprocess,      │ kimi-cli tests_e2e/ + insta crate for snapshots                                                 │
│      │          │ talk JSON, snapshot responses)                          │                                                                                                 │
│ 2    │ High     │ TestRig-style harness (in-process, recorded LLM         │ gemini-cli @google/gemini-cli-test-utils → coderex `tests-utils` crate                          │
│      │          │ response fixtures)                                      │                                                                                                 │
│ 3    │ High     │ Eval framework (.eval.rs files, fake LLM responses,     │ gemini-cli evals/                                                                               │
│      │          │ assertions on tool calls and file outputs)              │                                                                                                 │
│ 4    │ Med      │ Performance regression suite using criterion +          │ gemini-cli perf-tests/                                                                          │
│      │          │ baselines.json with tolerance                           │                                                                                                 │
│ 5    │ Med      │ Memory regression suite using dhat-rs                   │ gemini-cli memory-tests/                                                                        │
│ 6    │ Med      │ Drift detection workflow (compares behaviour across     │ gemini-cli agent-session-drift-check.yml                                                        │
│      │          │ PRs)                                                    │                                                                                                 │
│ 7    │ Low      │ Terminal-bench integration (industry SWE-bench-style)   │ qwen-code terminal-bench                                                                        │
│ 8    │ Low      │ Deflake retry workflow                                  │ gemini-cli deflake.yml                                                                          │
└──────┴──────────┴─────────────────────────────────────────────────────────┴─────────────────────────────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Best-design summary (per module)

```
┌──────────────────────────┬─────────────────────────┬─────────────────────────────────────────────────────────────────────────────────────────────────┐
│ Module                   │ Best reference          │ One-liner                                                                                       │
├──────────────────────────┼─────────────────────────┼─────────────────────────────────────────────────────────────────────────────────────────────────┤
│ Query engine / scheduler │ gemini-cli              │ scheduler/ with state-manager + parallel + waiting-callback                                     │
│ Tool registry            │ coderex (tiering) +     │ ToolTier + jit-context + modifiable-tool                                                        │
│                          │ gemini-cli (mechanics)  │                                                                                                 │
│ Hooks                    │ qwen-code               │ 6-layer + ssrfGuard + envInterpolator                                                           │
│ Agents                   │ gemini-cli              │ 6 specialised agents + agent-scheduler + a2a-server                                             │
│ Memory                   │ coderex (breadth) +     │ team_memory + auto_dream + hierarchical snippets/topics                                         │
│                          │ gemini-cli (structure)  │                                                                                                 │
│ MCP                      │ qwen-code (auth) +      │ Google OAuth + SA impersonation + token storage; coderex adds proxy                             │
│                          │ coderex (proxy)         │                                                                                                 │
│ Bridge / remote          │ coderex                 │ 7-module decomposition                                                                           │
│ UI / IDE                 │ qwen-code (breadth) +   │ vscode + zed + webui; ACP wire protocol                                                         │
│                          │ kimi-cli (protocol)     │                                                                                                 │
│ Config / sandbox         │ gemini-cli              │ policy engine + 3-OS sandbox + integrity                                                        │
│ Safety / permissions     │ gemini-cli (stack) +    │ confirmation-bus + safety checkers; shell-semantics ast parsing                                 │
│                          │ qwen-code (shell)       │                                                                                                 │
│ Model routing            │ gemini-cli              │ routing + availability + fallback                                                               │
│ Plugins / skills         │ gemini-cli              │ skill-extraction-agent + skillLoader + skillManager                                             │
│ SDK                      │ gemini-cli (api) +      │ broad typed surface; multi-language                                                             │
│                          │ qwen-code (langs)       │                                                                                                 │
│ Telemetry                │ gemini-cli              │ OTel-style with multiple exporters                                                              │
│ Testing / e2e            │ gemini-cli              │ TestRig + 30 evals + memory/perf baselines + drift checks                                       │
└──────────────────────────┴─────────────────────────┴─────────────────────────────────────────────────────────────────────────────────────────────────┘
```

---

## 4. Recommendations for coderex (prioritised)

### High priority — close major gaps

1. **Sandbox crate** ← gemini-cli `sandbox/` (macOS seatbelt, Linux seccomp+landlock, Windows AppContainer). Move shell/edit/write tools behind it by default.
2. **Hook security hardening** ← qwen-code `ssrfGuard.ts` + `urlValidator.ts` + `envInterpolator.ts`. Port to `crates/hooks/`.
3. **E2E test framework** ← combine kimi-cli wire-protocol pattern with gemini-cli `TestRig` (recorded LLM fixtures). Add a `tests-utils` workspace crate.
4. **AI eval framework** ← gemini-cli `evals/`. Define `evalTest!()` macro that asserts on tool calls + file outputs against a fake-response transcript.
5. **VS Code extension** ← kimi-cli ACP wire protocol shape. Define a documented JSON wire protocol so the same protocol powers IDE + Web UI.
6. **Multi-provider via Provider trait** ← gemini-cli `routing/` + `availability/` + `fallback/`. Refactor `core::api_client` behind a `Provider` trait.

### Medium priority — adopt best-of-breed designs

7. **Hierarchical memory** ← gemini-cli snippets/topics. Layer structured retrieval keys on `long_term.rs`.
8. **Confirmation bus** ← gemini-cli `confirmation-bus/`. Decouples permission UI from tool execution (prerequisite for IDE/Web UI).
9. **Shell safety** ← qwen-code `shell-semantics.ts`. Parse bash AST to classify destructive intent before approving.
10. **Skill extraction agent** ← gemini-cli `skill-extraction-agent`. Auto-distil reusable skills during compaction.
11. **Specialised agent registry** ← gemini-cli `agents/registry.ts` + agent types (codebase-investigator, memory-manager).
12. **Performance + memory baselines** ← gemini-cli `perf-tests/` + `memory-tests/`. Use `criterion` + `dhat-rs`.
13. **Telemetry exporters** ← gemini-cli `telemetry/`. Wire `opentelemetry-rust` with file + OTLP exporters.

### Low priority — polish + reach

14. **A2A server** ← gemini-cli `a2a-server` package. Optional companion crate for cross-process agent mesh.
15. **Multi-language SDK** ← qwen-code `sdk-typescript` + `sdk-java`. Generate from Rust SDK via `pyo3` + `napi-rs`.
16. **Web UI** ← qwen-code `webui` package. Single-page React UI talking the wire protocol from rec #5.
17. **DevTools UI** ← gemini-cli `devtools` package. In-app inspector for debugging hooks/tools/MCP.

---

## 5. coderex unique strengths to preserve

- **Rust performance and binary distribution** — no other agent has this. Startup, memory footprint, and single-binary deployability are differentiators.
- **`ToolTier` typed gating** — the cleanest tool-classification system in the field. Keep extending the enum rather than flattening.
- **Bridge (7-module decomposition)** — the cleanest remote-control implementation of all five.
- **Team memory + auto-dream + transcript** — unique memory capabilities. None of the open-source agents have shared memory or background distillation.
- **Enterprise config (`trusted_device` + `org_policy` + `remote_settings` + checksum-signed managed settings)** — currently ahead of all four open-source alternatives; matches cc-src's enterprise posture.
- **Spec-driven development (`spec.md` + `tasks.md`)** — provides a clear contract that none of the other projects have. Keep treating spec.md as the source of truth.
