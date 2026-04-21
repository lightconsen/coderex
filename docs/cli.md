# cli Module

## Goal

The main binary crate. Parses CLI arguments, bootstraps the REPL, wires together all subsystems (core engine, UI, commands, tools, memory, config), and runs the main event loop.

## Key Types

- **`Cli`** (`cli.rs`) — Clap argument struct with `--directory`, `--permissions`, `--model`, `--print`, and subcommand.
- **`Command`** (`cli.rs`) — Subcommands: `Repl`, `Serve`, `ServeStop`, `ServeStatus`, `Resume { session_id }`.
- **`ReplLoop`** (`repl.rs`) — Main REPL event loop: reads keys, dispatches to Vim handler, updates UI state, orchestrates queries and commands.
- **`StartupOrchestrator`** (`orchestrator.rs`) — Bridges REPL to `QueryEngine`. Manages startup phases, API client creation, mode routing (TUI, headless, server, resume), telemetry init.

## Main Flow

```
main() → parse_args() → StartupOrchestrator::run()
  → init_telemetry() → crash report ingestion
  → update check (non-blocking, once per 24h)
  → signal handlers (SIGTERM: graceful shutdown, SIGHUP: config reload)
  → run() routes based on args:
    --print         → run_headless() → stdin line → single turn → print → exit
    Command::Repl   → run_repl() → TUI
    Command::Serve  → run_server() → WebSocket daemon (writes PID file)
    Command::ServeStop  → run_serve_stop() → sends SIGTERM to daemon
    Command::ServeStatus → run_serve_status() → reports daemon state
    Command::Resume → run_resume() → load session transcript → TUI
    (no flag/cmd)   → run_repl() → TUI

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

- Full CLI argument parsing with clap (`--directory`, `--permissions`, `--model`, `--print`).
- TUI REPL with Vim mode (normal/insert), ratatui rendering, status line, message list.
- Streaming API integration with real-time token display.
- Tool execution with permission dialogs and AI-powered permission explanations.
- Session transcript persistence to `.coderex/transcripts/`.
- Settings loading from `~/.coderex/settings.json` and `.coderex/settings.json`.
- Tracing setup with JSON formatter option.
- Welcome screen with T-Rex ASCII art.
- Ctrl+C interrupt handling.
- Non-interactive headless mode (`--print`): reads one line from stdin, runs a single turn, prints response, exits.
- Crash telemetry: SQLite store ingests pending panic hooks at startup.
- Session resume (`coderex resume <session_id>`): loads transcript, continues conversation.
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
- Terminal guard: detects `!stdin.is_terminal()` and rejects non-TTY for REPL/resume, allowing headless mode via `--print`.
