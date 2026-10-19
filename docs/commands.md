# commands Module

## Goal

Implements the `/` slash command system that users type in the REPL. Provides a registry with type-safe dispatch across 77 commands covering sessions, configuration, diagnostics, Git integration, context management, macros, history, and advanced features.

## Design

### Key Types

- **`CommandRegistry`** (`registry.rs`) — Vec-based registry mapping command names to handler closures. Supports `find`, `list`, `execute`, and availability filtering. Validates argument count before dispatch via `CommandArgs`.
- **`CommandContext`** (`registry.rs`) — Per-invocation context: cwd, model, session state, token usage, cost, feature flags, git branch, theme, vim mode, env vars, and `pipe_input: Option<String>` for command chaining.
- **`CommandHandler`** (`registry.rs`) — `Arc<dyn Fn(&CommandContext, &[String]) -> CommandResult + Send + Sync>`.
- **`SlashCommand`** (`registry.rs`) — Metadata: name, description, kind (`Local`/`LocalUi`/`Prompt`/`Standalone`), `supports_non_interactive`, `remote_safe`, `bridge_safe`, `uses_agent_backend`, `destructive: bool`, `args: CommandArgs`, handler.
- **`CommandResult`** (`registry.rs`) — Output, `state_changed`, `suppress_output`, `extra_context`, `append_system_prompt`, `override_system_prompt`, `prompt_profile`, `allowed_tools_override`, `scrollable: bool`.
- **`CommandArgs`** (`registry.rs`) — Argument validation enum: `None` (no validation), `AtLeast(n)`, `AtMost(n)`, `Exactly(n)`. Validated pre-dispatch in `execute()`.
- **`CommandHistory`** (`history.rs`) — Ring buffer (max 1000) with JSONL file persistence (`~/.coderex/command_history.jsonl`). Tracks `{cmd, args, timestamp}` per entry. Supports `append`, `list(n)`, `search(query)`.
- **`MacroRegistry`** (`macro_commands.rs`) — User-defined command sequences as `HashMap<String, Vec<String>>` with JSON persistence (`~/.coderex/macros.json`). Supports `define`, `delete`, `get`, `list`, `load`.

### Command Inventory (77 commands across 13 registered modules)

| Module | Commands | Purpose |
|--------|----------|---------|
| `session_commands` | `/clear`, `/exit`, `/session` | Session lifecycle |
| `config_commands` | `/config`, `/model`, `/permissions` | Core configuration |
| `stats_commands` | `/stats`, `/cost`, `/usage` | Usage statistics |
| `content_commands` | `/compact`, `/help`, `/version` | Session content & help |
| `session_extension_commands` | `/resume`, `/rename`, `/rewind` | Session persistence & manipulation |
| `config_extension_commands` | `/theme`, `/hooks`, `/mcp`, `/plugin`, `/memory`, `/skills`, `/tasks`, `/plan` | Extended configuration |
| `diagnostic_commands` | `/status`, `/doctor` | System health checks |
| `context_commands` | `/copy`, `/init`, `/summary`, `/files`, `/context`, `/add-dir` | Context management |
| `integration_commands` | `/ide`, `/chrome`, `/desktop`, `/mobile`, `/voice`, `/vim`, `/remote-env`, `/remote-setup`, `/statusline-setup` | External integrations |
| `git_commands` | `/commit`, `/commit-push-pr`, `/review`, `/security-review`, `/autofix-pr`, `/branch` | Git & code review |
| `debug_commands` | `/debug-tool-call`, `/heapdump`, `/mock-limits`, `/perf-issue`, `/ant-trace`, `/break-cache`, `/reload-plugins` | Debugging tools |
| `advanced_commands` | `/bridge`, `/bridge-kick`, `/teleport`, `/ultraplan`, `/insights`, `/thinkback`, `/thinkback-play`, `/statusline` | Advanced/feature-gated |
| `misc_commands` | `/advisor`, `/btw`, `/good-claude`, `/stickers`, `/bughunter`, `/issue`, `/env`, `/diff`, `/oauth-refresh`, `/backfill-sessions`, `/history` | Miscellaneous. `/history [n]` and `/history search <q>` read the session's recorded command lines. |
| `macro_commands` | `/macro` (REPL built-in) | `MacroRegistry` loads, persists and expands macros. `/macro define <name> <command[; command…]>`, `list`, `run <name>`, `delete <name>` are handled by the REPL; a defined macro also runs by typing `/<name>`. |

### Architecture Patterns

- **Registry pattern** — Commands register at startup via `register_all_commands()` called from `registry.rs`.
- **Closure-based handlers** — Each command is an `Arc<dyn Fn>` with shared `CommandContext`.
- **Availability matrix** — Commands filtered by `supports_non_interactive`, `remote_safe`, `bridge_safe` for different execution contexts.
- **Agent backend flag** — Commands can opt into sub-agent execution via `with_agent_backend()`.
- **Argument validation** — `CommandArgs` enum validated pre-dispatch; `CommandArgs::None` accepts any count, while `AtLeast(n)`, `AtMost(n)`, `Exactly(n)` enforce constraints. Production declarers: `/btw` and `/add-dir` require at least one argument, `/rename` at least one word (titles may contain spaces), and `/model`, `/theme` and `/resume` accept at most one. `/history` keeps form-specific checks because `search` takes a multiword query.
- **Destructive confirmation** — Commands marked `.with_destructive()` trigger a `Confirm` overlay in TUI mode (y/n), skipped in headless mode.
- **Command piping** — Input split on `|` operator; segments executed sequentially with output of each passed as `pipe_input` to the next via `CommandContext`.
- **Macro expansion** — Command names resolved against `MacroRegistry` before registry dispatch; macro sequences expanded and executed in order.
- **Scrollable output** — `CommandResult.scrollable` flag causes TUI to render output as an `Assistant` message (scrollable in message list) rather than a single-line `System` message.
- **Command history** — Every command execution appended to `CommandHistory` ring buffer with file persistence; `/history` command queries and searches.
- **Modular organization** — Commands grouped by domain across 13 registered modules, each with a `register(&mut CommandRegistry)` function.

## Current Status

### What Works

- Full command registry with name-to-handler mapping, argument validation, and availability filtering.
- `/clear`, `/exit`, `/help`, `/version`, `/compact` fully functional. `/clear` requires confirmation in TUI mode.
- `/model` switches active model; `/permissions` manages permission mode.
- `/cost`, `/stats`, `/usage` report token usage and cost breakdown.
- Git commands (`/commit`, `/commit-push-pr`, `/review`, `/branch`) work via git CLI and prompt profiles.
- `/theme` lists and changes themes with available theme detection.
- `/status` and `/doctor` show comprehensive system health (API key, MCP, git, model, feature flags).
- `/memory` reads and searches long-term memory entries.
- `/hooks`, `/mcp`, `/plugin`, `/skills`, `/tasks` list and manage subsystem state. `/plugin components` lists the themes, output styles, workflows, monitors, LSP servers, channels, binaries, types and experimental data contributed by active plugins.
- `/style [name]` lists or applies a plugin output style as additional system instructions.
- `/resume [id|title fragment]` accepts an exact session id; anything else is resolved through the session index, and an ambiguous fragment lists the candidates instead of guessing.
- Feature-gated commands (`/ultraplan`, `/thinkback`, `/thinkback-play`, `/voice`, `/ctx_viz`, `/heapdump`, `/mock-limits`, `/stickers`) check `CommandContext.feature_flags`, which the REPL now fills from the resolved `FeatureFlags` (env + `--betas`).
- `/bridge` and `/bridge-kick` check `CODEREX_BRIDGE_URL` env var.
- `/insights` aggregates project file counts, git info, and session token usage.
- Command chaining via `|` operator (e.g., `/diff | /commit`) passes output between commands.
- Command history is recorded for built-ins and registry commands alike (both REPL paths record before dispatch) and persisted; `/history` reads it, with `[n]` and `search <query>`.
- `/macro define|list|run|delete` is a REPL built-in backed by `~/.coderex/macros.json`; macro names also expand directly.
- All commands have basic error handling with usage messages.

### Feature-Gated Commands

Several commands require feature flags to be enabled in `CommandContext`:
- `/ultraplan` — needs `ultraplan`
- `/thinkback` / `/thinkback-play` — needs `thinkback`
- `/voice` — needs `voice_mode`
- `/ctx_viz` — needs `ctx_viz`
- `/heapdump` — needs `heapdump`
- `/mock-limits` — needs `mock_limits`
- `/stickers` — needs `stickers`

These return an error message when the feature is not enabled. The flag names are accepted by `CODEREX_FEATURE_FLAGS` and `--betas`.

## Remaining Gaps

- **`/macro` is a REPL built-in, not a registry command.** It mutates the session's
  `MacroRegistry` and can therefore run in the TUI and headless paths, but remote/bridge
  sessions (which dispatch through the registry) do not see it.
- **Remote/bridge availability filtering is only used by the server.** The local TUI does not consult the availability matrix.
