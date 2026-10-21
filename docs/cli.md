# cli Module

## Goal

The main binary crate. Parses CLI arguments, bootstraps the REPL, wires together all subsystems (core engine, UI, commands, tools, memory, config), and runs the main event loop.

## Key Types

- **`Cli`** (`cli.rs`) — Clap argument struct with 40+ flags and subcommands matching Claude Code's interface.
- **`Command`** (`cli.rs`) — Subcommands: `Repl`, `Serve`, `ServeStop`, `ServeStatus`, `Agents`, `Auth`, `Doctor`, `Update`, `Mcp`, `Plugin`, `AutoMode`, `Telemetry` (`preview-crash`), `Bridge`.
- **`ReplLoop`** (`repl.rs`) — Main REPL event loop: reads keys, dispatches to Vim handler, updates UI state, orchestrates queries and commands.
- **`StartupOrchestrator`** (`orchestrator.rs`) — Bridges REPL to `QueryEngine`. Manages startup phases, API client creation, mode routing (TUI, headless, server, resume), telemetry init, and CLI flag application.

## Main Flow

```
main() → parse_args() → StartupOrchestrator::run()
  → init_telemetry() → crash report ingestion
  → update check (non-blocking, once per 24h)
  → signal handlers (SIGTERM: graceful shutdown, SIGHUP: config reload)
  → run() routes based on args:
    --print / PROMPT  → run_headless() → single turn → print → exit
    Command::Repl     → run_repl() → TUI
    Command::Serve    → run_server() → WebSocket daemon (writes PID file)
    Command::ServeStop    → run_serve_stop() → sends SIGTERM to daemon
    Command::ServeStatus  → run_serve_status() → reports daemon state
    Command::Agents       → list available agents
    Command::Auth         → show API provider status
    Command::Doctor       → run diagnostics
    Command::Update       → check for updates
    Command::Mcp          → manage MCP servers
    Command::Plugin       → manage plugins
    Command::AutoMode     → show auto-mode settings
    --continue / --resume → load session transcript → TUI
    (no flag/cmd)         → run_repl() → TUI

  TUI path:
  → configure_repl() applies all CLI flags to ReplLoop
    → apply_cli_config() sets permission mode, session name, worktree, fork, etc.
  → TerminalUi::enter() → event loop
    → crossterm key events → VimState::handle()
      → Insert mode: accumulates input, /command detection
      → Normal mode: navigation, scroll, search
    → On Enter: submit to QueryEngine.submit_streaming()
    → Engine events → UiState updates → TerminalUi::draw()
    → Background tasks polled: prompt suggestions, memory updates, agent summaries
    → Config changes polled (hot-reload feature)
```

## CLI Arguments

### Global Flags

| Flag | Short | Description |
|------|-------|-------------|
| `--directory DIR` | | Working directory (defaults to current directory) |
| `--add-dir DIR` | `-a` | Additional directories to include in context (contents listed and injected) |
| `--permissions MODE` | | Permission mode: `default`, `accept-edits`, `plan`, `dont-ask` |
| `--dangerously-skip-permissions` | | Skip all permission prompts (sets `BypassPermissions` mode) |
| `--allowed-tools TOOLS` | | Comma-separated list of allowed tools (restricts tool executor) |
| `--model MODEL` | `-m` | Model to use (e.g., `claude-sonnet-4-6`, `claude-opus-4-6`) |
| `--fallback-model MODEL` | | Retry the same request once with this model when the primary is unavailable; if that succeeds the session continues on the fallback. Works in interactive and headless mode; other errors never trigger it |
| `--system-prompt PROMPT` | | Custom system prompt (replaces default), in headless and interactive mode |
| `--append-system-prompt TEXT` | | Append TEXT to the default system prompt instead of replacing it. Independent of `--system-prompt`; when both are given, the replacement is authoritative and TEXT is added as extra instructions |
| `--file FILE` | `-f` | Include a file in context (can be repeated) |
| `--print` | `-p` | Non-interactive mode: read from stdin, print response, exit |
| `--brief` | | Concise output (minimal formatting) |
| `--bare` | | Minimal output (no decorations) |
| `--verbose` | | Verbose output with more detail |
| `--debug` | `-d` | Enable debug logging |
| `--debug-file PATH` | | Write debug output to a file (redirects tracing appender) |
| `--continue` | `-c` | Continue the most recent session |
| `--resume ID` | `-r` | Resume a specific session by ID |
| `--name NAME` | `-n` | Session name (sets `SessionState.title`) |
| `--fork-session ID` | | Fork a session from an existing session ID (loads transcript messages) |
| `--from-pr URL` | | Fetch a PR with `gh` and prepend its context to the initial prompt (see below) |
| `--no-session-persistence` | | Disable session transcript saving (clears memory stores) |
| `--disable-slash-commands` | | Disable slash commands in the REPL: clears `CommandRegistry`, skips the REPL's built-in command branches in both interactive and headless paths, and stops macro expansion |
| `--max-budget-usd USD` | | Maximum budget for this session |
| `--api-key KEY` | | API key (overrides environment and config) |
| `--no-telemetry` | | Disable telemetry for this session |
| `--mcp-config PATH` | | MCP config file path |
| `--mcp-debug` | | Enable MCP debug mode |
| `--strict-mcp-config` | | Strict MCP config validation (fail on unknown fields) |
| `--plugin-dir DIR` | | Additional plugin directory |
| `--settings PATH` | | Settings file path (overrides default) |
| `--setting-sources` | | Show where each setting value comes from |
| `--input-format FORMAT` | | Input format: `text` (default) or `json` (parses `{"prompt":"..."}`) |
| `--output-format FORMAT` | | Output format: `text` (default), `json`, `markdown`, `stream-json` (one protocol frame per line) |
| `--json-schema SCHEMA` | | JSON schema for structured output (passed to `SubmitOptions`) |
| `--replay-user-messages` | | Replay user messages from most recent session transcript |
| `--include-partial-messages` | | Include partial assistant messages in context |
| `--effort LEVEL` | | Reasoning effort: `low`, `medium`, or `high` (case-insensitive). Any other value is rejected at startup. The level is carried on every request context and mapped per provider — see "Reasoning effort" below |
| `--betas FLAGS` | | Comma-separated feature flags (`+name`, `-name`, or bare `name`) applied to the session |
| `--chrome` | | Enable browser / web integration |
| `--no-chrome` | | Disable browser / web integration (removes `ChromeClient` from dispatcher) |
| `--tmux` | | Run in tmux: re-launch in a new session outside tmux, set the title inside tmux |
| `--agent AGENT` | | Use a named built-in or custom agent definition for the session |
| `--ide IDE` | | Connect to a configured IDE MCP server and register its tools |
| `--worktree` | `-w` | Worktree mode (detects git branch, sets `WorktreeState`) |

### Subcommands

| Command | Description |
|---------|-------------|
| `repl` | Start the interactive REPL (default) |
| `serve [--port PORT]` | Start WebSocket daemon server |
| `serve-stop` | Stop the running daemon |
| `serve-status` | Show daemon status |
| `agents [--detailed]` | List available agents |
| `auth` | Check authentication status |
| `doctor` | Run diagnostics |
| `update` | Check for updates |
| `mcp list` | List configured MCP servers |
| `mcp add NAME COMMAND` | Add an MCP server |
| `mcp remove NAME` | Remove an MCP server |
| `mcp test NAME` | Test an MCP server connection |
| `plugin list` | List installed plugins |
| `plugin install SOURCE` | Install a plugin |
| `plugin uninstall NAME` | Uninstall a plugin |
| `plugin update` | Update all plugins |
| `auto-mode` | Show auto-mode settings (reads `dream_config.json`) |
| `telemetry preview-crash [FILE]` | Print the JSON that would be uploaded for a pending crash (never uploads) |

### Reasoning effort

`--effort low|medium|high` asks the provider to spend a matching amount of
reasoning on each turn. The value is stamped onto every request context by an
`EffortClient` that wraps the provider router, so it reaches whichever provider
serves the turn, and each provider maps it onto the control its API actually
exposes:

| Provider profile | Mapping |
|---|---|
| Anthropic (`anthropic`, and cloud endpoints such as Bedrock/Vertex that use the Anthropic Messages shape) | `thinking: {"type": "enabled", "budget_tokens": N}` — `low` = 1024, `medium` = 4096, `high` = 16384. `max_tokens` is raised above the budget when the caller's ceiling is lower, because Anthropic rejects a body where it is not. |
| OpenAI and Groq | `reasoning_effort: "<level>"` on Chat Completions; `reasoning: {"effort": "<level>"}` on the Responses API. |
| Kimi, DashScope, and unnamed OpenAI-compatible endpoints | No verified equivalent. The field is **omitted** and a warning names the provider — the request is never silently reshaped, and the choice is not silently dropped. |

The effort is carried on `RequestContext` under the `reasoning_effort` provider
hint, and a per-turn value on the context wins over a client-level default. An
unknown level (`--effort maximum`) is rejected at startup with the accepted
values.

### Telemetry inspection

`coderex telemetry preview-crash` loads the newest pending crash from
`<config-home>/crashes/pending/` (or the file given as its argument), converts
it with the same modes ingestion uses, and prints the exact wire payload:

```bash
coderex telemetry preview-crash
coderex telemetry preview-crash /path/to/crash_1729.json
CODEREX_CRASH_VERBOSE=1 coderex telemetry preview-crash
CODEREX_HIPAA=1 coderex telemetry preview-crash
```

The file is read, not consumed: nothing is ingested, deleted, or uploaded.
`CODEREX_HIPAA` wins over `CODEREX_CRASH_VERBOSE`, matching the uploader. See
`docs/telemetry.md` for the payload fields and redaction markers.

### Positional Arguments

```
coderex [OPTIONS] [COMMAND] [PROMPT...]
```

A positional `PROMPT` argument runs in non-interactive mode (same as `--print`). Multiple words are joined into a single prompt.

## Context and Integration Flags

### `--betas FLAGS`

A comma-separated list applied on top of `CODEREX_FEATURE_FLAGS`:

- `+name` enables, `-name` disables, and a bare `name` enables.
- The result is stored as the session's `FeatureFlags`, and its
  `CapabilitySet` is derived from it.
- Every name must be one this build knows: `streaming_tool_execution`,
  `tool_use_summaries`, `fast_mode`, `coordinator_mode`, `history_snip`,
  `agent_triggers`, `voice_mode`, `workflow_scripts`, `web_browser_tool`,
  `team_collaboration`, `auto_dream`, and the six command-availability flags
  (`ultraplan`, `thinkback`, `heapdump`, `mock_limits`, `ctx_viz`, `stickers`).
- An unknown name is a startup error listing the unknown names and the known
  set; it is never dropped silently.
- `INERT_FLAGS` is empty: every known name now has a consumer. A name that
  cannot be gated at runtime should be deleted rather than accepted, because
  accepting it promises behavior that does not exist.
- Six names that used to be accepted were removed. `context_collapse`,
  `terminal_panel`, and `kairos` had no product surface anywhere in the
  workspace. `bridge_mode`, `daemon`, and `buddy` were removed after review
  for a related reason — no runtime-gateable surface: the bridge and the daemon
  are separate subcommand
  entry points (`bridge`, `serve`) with no session-level behavior a flag could
  gate, and the companion lives behind the compile-time `buddy` cargo feature,
  which a runtime flag cannot reach. All six are rejected as unknown flags.
- These names do have consumers: `web_browser_tool` (gates the `WebFetch`
  tool), `agent_triggers` (gates the agent-kind hook path — with it off a
  configured `agent` hook is dropped at startup with a warning), `auto_dream`
  (gates the auto-dream manager — with it off `/auto-dream` reports as
  uninitialized), `fast_mode` (resolves the session model through the
  lightweight profile when nothing else chose one — an explicit `--model`,
  `ANTHROPIC_MODEL`/`OPENAI_MODEL`, or settings `model` still wins),
  `coordinator_mode` (installs a `CoordinatorState` on the session engine so
  its per-turn phase advance runs), `history_snip` (makes compaction prefer the
  snip strategy as soon as it is warranted), `streaming_tool_execution`
  (on: tool calls execute while the stream is open; off, the default: they
  execute after the stream completes), `tool_use_summaries` (asks the
  lightweight model for a one-line label per tool use and emits it as an
  engine event, falling back to the deterministic activity summary on
  failure), `team_collaboration` (installs a `TeamContext` built from stored
  team-memory entries, whose notes the engine injects into each request),
  `workflow_scripts`, `voice_mode`, and the six command-availability flags.

### `--from-pr URL`

Fetches pull-request context with the GitHub CLI and puts it at the front of
the initial prompt (headless) or into the session's extra context
(interactive).

Requirements:

- `gh` must be installed and on `PATH`.
- `gh` must be authenticated, via `gh auth login` or `GH_TOKEN` in the
  environment. The variable is inherited by the child process; CodeRex does
  not read or rewrite it.
- The URL must have the shape
  `https://<host>/<owner>/<repo>/pull/<number>` (GitHub Enterprise hosts are
  allowed).

Commands run:

```text
gh pr view <url> --json title,body,files,commits
gh pr diff <url>
```

The rendered `<pr_context>` block contains the title, description (max 4,000
characters), a changed-file summary (max 200 files), commit subjects (max 100),
and the diff (max 60,000 characters), with explicit truncation markers. A
missing `gh`, auth failure, invalid URL, parse failure, or failed command
stops startup with the CLI's message; CodeRex does not continue without the
context.

### `--tmux`

- **Outside tmux** (`$TMUX` unset): requires `tmux` on `PATH` and an
  interactive terminal. CodeRex starts `tmux new-session -s coderex-<pid>`
  with the current executable and the original arguments minus `--tmux`.
  The child gets `CODEREX_TMUX_ACTIVE=1`; together with `$TMUX` this prevents
  a re-exec loop. When the tmux client exits, CodeRex exits with its status.
- **Inside tmux** (`$TMUX` set, or `CODEREX_TMUX_ACTIVE` set): no nested
  session is created. CodeRex renames the window to `coderex: <directory>`,
  sets the pane title, and turns `automatic-rename` off.
- If `tmux` is not available, `--tmux` fails with an actionable message.

### `--ide IDE`

CodeRex does not bundle an IDE. `--ide` connects to a configured IDE MCP
server and registers the tools it exposes (`executeCode`, `getDiagnostics`,
and anything else the server lists) into the same registry the model
dispatches through. The connection status and discovered tool names are
reported on stderr and in the log.

Configuration is resolved in this order:

1. `CODEREX_IDE_MCP_URL` — a remote HTTP/SSE MCP endpoint.
2. `CODEREX_IDE_MCP_COMMAND` — a stdio MCP server command line, split on
   whitespace (`"ide-mcp --stdio"`).
3. `mcp_servers` (or `mcpServers`) in settings, looked up first under the
   `--ide` value (for example `vscode`) and then under `ide`.
4. `.mcp.json` in the working directory (or the `--mcp-config` file), with
   the same name lookup.

Example settings entry:

```json
{
  "mcp_servers": {
    "ide": {
      "type": "stdio",
      "command": "/usr/local/bin/ide-mcp",
      "args": ["--stdio"]
    }
  }
}
```

If no server is configured, the configured server cannot be started, or it
answers `tools/list` with no tools, startup fails with a message naming the
server and the configuration source. `--ide` is never a silent no-op.

## CLI Flag Application Architecture

CLI flags are parsed by clap, stored in `SessionContext` during `StartupOrchestrator::run()`, and applied to the `ReplLoop` via `configure_repl()`. The `ReplLoop::apply_cli_config()` method performs the actual side effects:

1. **Permission mode** — `--dangerously-skip-permissions` sets `PermissionMode::BypassPermissions` on the engine's `PermissionChecker`.
2. **Slash commands** — `--disable-slash-commands` calls `CommandRegistry::clear()`; the REPL additionally gates its built-in command branches and macro expansion on the same flag in both the TUI and headless paths.
3. **Chrome** — `--no-chrome` calls `RegistryToolDispatcher::clear_chrome_client()`.
4. **Session name** — `--name` sets `SessionState.title` on the engine.
5. **Worktree** — `--worktree` detects the current git branch and sets `WorktreeState`.
6. **Fork session** — `--fork-session` loads transcript messages into the engine session.
7. **Replay messages** — `--replay-user-messages` loads user messages from the most recent transcript.
8. **Add directories** — `--add-dir` reads directory listings into `session_effects.extra_context`.
9. **Beta flags** — `--betas` parses `+name`/`-name`/`name` over `CODEREX_FEATURE_FLAGS`, stores the result as the session `FeatureFlags`, and derives the session `CapabilitySet`. Unknown names fail startup.
10. **PR context** — `--from-pr` is fetched during orchestrator startup and prepended to the initial prompt (headless) or added to `session_effects.extra_context` (interactive).
11. **IDE** — `--ide` connects to a configured IDE MCP server during startup and registers its discovered tools into the dispatcher registry.
12. **tmux** — `--tmux` is applied before any startup phase; outside tmux it re-launches the binary in a new session.
13. **JSON schema** — `--json-schema` is parsed and passed to `SubmitOptions`.
14. **Partial messages** — `--include-partial-messages` is passed to `SubmitOptions`.
15. **Debug file** — `--debug-file` is handled in `phase_bootstrap()` by redirecting the tracing subscriber to a file appender.
16. **Persistence** — `--no-session-persistence` clears `session_memory_store`, `long_term_store`, `auto_dream`, and `team_memory_service`.
17. **Allowed tools** — `--allowed-tools` sets `session_effects.allowed_tools_override`.

## Architecture Patterns

- **tokio async runtime** — `#[tokio::main]` multi-threaded runtime for all async operations.
- **Channel-based communication** — `mpsc::unbounded_channel` for engine events and background task results.
- **`Arc<Store>`** — Shared mutable state via `Arc<SessionMemoryStore>` and `Arc<LongTermStore>` wired at startup.
- **Graceful shutdown** — Ctrl+C via `crossterm` raw mode guard, SIGTERM/SIGHUP via `tokio::signal::unix`, TUI cleanup on exit.
- **Telemetry** — SQLite-backed crash telemetry store with pending crash report ingestion at startup.
- **Crash recovery** — Session state snapshot (`.coderex/recovery.json`) written periodically, restored via `/recover`.
- **Daemon management** — PID file (`~/.coderex/daemon.pid`) with `serve-stop`/`serve-status` subcommands.
- **Config hot-reload** — `ConfigWatcher` under `hot-reload` feature, polls file changes in settings JSON.
- **Update check** — GitHub Releases API comparison with `CARGO_PKG_VERSION`, 24h cadence, `/update-check` command.

## Implemented Features

- Full CLI argument parsing with clap (40+ flags, 10+ subcommands) — all flags have real implementations, no stubs.
- TUI REPL with Vim mode (normal/insert), ratatui rendering, status line, message list.
- Streaming API integration with real-time token display.
- Tool execution with permission dialogs and AI-powered permission explanations.
- Session transcript persistence to `.coderex/transcripts/`.
- Settings loading from `~/.coderex/settings.json` and `.coderex/settings.json`.
- Tracing setup with JSON formatter option and `--debug-file` file appender.
- Welcome screen with T-Rex ASCII art.
- Ctrl+C interrupt handling.
- Non-interactive headless mode (`--print` or positional prompt): reads input, runs a single turn, prints response, exits.
- Crash telemetry: SQLite store ingests pending panic hooks at startup.
- Session resume (`--resume ID` or `--continue`): loads transcript, continues conversation.
- WebSocket server daemon (`coderex serve`): writes PID file on start, removes on exit.
- Daemon management: `coderex serve-stop` sends SIGTERM, `coderex serve-status` reports liveness.
- Crash recovery: `.coderex/recovery.json` snapshot, `/recover` command.
- Config hot-reload: `ConfigWatcher` under `hot-reload` feature (`cargo build --features hot-reload`).
- Multi-workspace: `/workspace [path]` switches CWD, validates directory.
- Update check: non-blocking GitHub release comparison on startup, `/update-check` command.
- Signal handling: SIGTERM (graceful shutdown, cleanup PID/recovery), SIGHUP (config reload trigger).
- Auto-dream: background session consolidation (`/auto-dream enable|disable|status`).
- Team memory sync: cross-repo memory sharing (`/team-memory upsert|read|list|delete|sync`).
- Background AI features: `/compact`, `/agent-summary`, `/away-summary`, `/magic-docs`, `/codebase-overview`, `/generate-readme`, `/agent-generation`, `/update-memory`, `/extract-memories`.
- Prompt profiles: `/advisor`, `/bughunter`, `/reviewer`.
- API key resolution: supports `keyring://` references for OS keychain lookup (compiled under `keyring` feature).
- Terminal guard: detects `!stdin.is_terminal()` and rejects non-TTY for REPL/resume, allowing headless mode via `--print` or positional args.
- Permission mode control: `--permissions` and `--dangerously-skip-permissions` set real `PermissionMode` on the engine.
- Chrome integration toggle: `--chrome`/`--no-chrome` controls `ChromeClient` attachment on the tool dispatcher.
- Worktree mode: `--worktree` detects current git branch and populates `WorktreeState`.
- Session forking: `--fork-session` loads existing transcript messages into the engine.
- User message replay: `--replay-user-messages` loads user messages from the most recent session.
- Structured output: `--json-schema` enables schema-validated JSON mode.
- Plugin command bridge: plugin-discovered commands are registered as `/plugin/command` slash commands.
- Plugin tool fallback: plugin tools are layered as a fallback behind the primary tool registry.
