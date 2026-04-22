# cli Module

## Goal

The main binary crate. Parses CLI arguments, bootstraps the REPL, wires together all subsystems (core engine, UI, commands, tools, memory, config), and runs the main event loop.

## Key Types

- **`Cli`** (`cli.rs`) — Clap argument struct with 30+ flags and subcommands matching Claude Code's interface.
- **`Command`** (`cli.rs`) — Subcommands: `Repl`, `Serve`, `ServeStop`, `ServeStatus`, `Agents`, `Auth`, `Doctor`, `Update`, `Mcp`, `Plugin`, `AutoMode`.
- **`ReplLoop`** (`repl.rs`) — Main REPL event loop: reads keys, dispatches to Vim handler, updates UI state, orchestrates queries and commands.
- **`StartupOrchestrator`** (`orchestrator.rs`) — Bridges REPL to `QueryEngine`. Manages startup phases, API client creation, mode routing (TUI, headless, server, resume), telemetry init.

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
| `--add-dir DIR` | `-a` | Additional directories to include in context |
| `--permissions MODE` | | Permission mode: `default`, `accept-edits`, `plan`, `dont-ask` |
| `--dangerously-skip-permissions` | | Skip all permission prompts (use with caution) |
| `--allowed-tools TOOLS` | | Comma-separated list of allowed tools |
| `--model MODEL` | `-m` | Model to use (e.g., `claude-sonnet-4-6`, `claude-opus-4-6`) |
| `--fallback-model MODEL` | | Fallback model if primary is unavailable |
| `--system-prompt PROMPT` | | Custom system prompt (replaces default) |
| `--append-system-prompt` | | Append to default system prompt instead of replacing |
| `--file FILE` | `-f` | Include a file in context (can be repeated) |
| `--print` | `-p` | Non-interactive mode: read from stdin, print response, exit |
| `--brief` | | Concise output (minimal formatting) |
| `--bare` | | Minimal output (no decorations) |
| `--verbose` | | Verbose output with more detail |
| `--debug` | `-d` | Enable debug logging |
| `--debug-file PATH` | | Write debug output to a file |
| `--continue` | `-c` | Continue the most recent session |
| `--resume ID` | `-r` | Resume a specific session by ID |
| `--name NAME` | `-n` | Session name (used for transcript naming) |
| `--fork-session ID` | | Fork a session from an existing session ID |
| `--from-pr URL` | | Load context from a PR URL |
| `--no-session-persistence` | | Disable session transcript saving |
| `--disable-slash-commands` | | Disable slash commands in the REPL |
| `--max-budget-usd USD` | | Maximum budget for this session |
| `--api-key KEY` | | API key (overrides environment and config) |
| `--no-telemetry` | | Disable telemetry for this session |
| `--mcp-config PATH` | | MCP config file path |
| `--mcp-debug` | | Enable MCP debug mode |
| `--strict-mcp-config` | | Strict MCP config validation |
| `--plugin-dir DIR` | | Additional plugin directory |
| `--settings PATH` | | Settings file path (overrides default) |
| `--setting-sources` | | Show where each setting value comes from |
| `--input-format FORMAT` | | Input format: `text` (default) or `json` |
| `--output-format FORMAT` | | Output format: `text` (default), `json`, `markdown` |
| `--json-schema SCHEMA` | | JSON schema for structured output |
| `--replay-user-messages` | | Replay user messages from previous session |
| `--include-partial-messages` | | Include partial assistant messages in context |
| `--effort LEVEL` | | Reasoning effort: `low`, `medium`, `high` |
| `--betas FEATURE` | | Enable beta features (can be repeated) |
| `--chrome` | | Enable browser / web integration |
| `--no-chrome` | | Disable browser / web integration |
| `--tmux` | | Enable tmux integration |
| `--agent AGENT` | | Use a specific agent |
| `--ide IDE` | | IDE integration mode (`vscode`, `jetbrains`, etc.) |
| `--worktree` | `-w` | Worktree mode (isolated git worktree) |

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
| `auto-mode` | Show auto-mode settings |

### Positional Arguments

```
coderex [OPTIONS] [COMMAND] [PROMPT...]
```

A positional `PROMPT` argument runs in non-interactive mode (same as `--print`). Multiple words are joined into a single prompt.

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

- Full CLI argument parsing with clap (30+ flags, 10+ subcommands).
- TUI REPL with Vim mode (normal/insert), ratatui rendering, status line, message list.
- Streaming API integration with real-time token display.
- Tool execution with permission dialogs and AI-powered permission explanations.
- Session transcript persistence to `.coderex/transcripts/`.
- Settings loading from `~/.coderex/settings.json` and `.coderex/settings.json`.
- Tracing setup with JSON formatter option.
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
