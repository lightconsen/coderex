# cli Module Inspection

## Goal

The main binary crate. Parses CLI arguments, bootstraps the REPL, wires together all subsystems (core engine, UI, commands, tools, memory, config), and runs the main event loop.

## Design

### Key Types

- **`Cli`** (`cli.rs`) — Clap-derived argument parser with subcommands (run, server, config, version).
- **`Command`** (`cli.rs`) — Subcommands: `Run`, `Server`, `Config`, `Version`.
- **`ReplLoop`** (`repl.rs`) — Main REPL event loop: reads keys, dispatches to Vim handler, updates UI state, sends to orchestrator.
- **`Orchestrator`** (`orchestrator.rs`) — Bridges REPL to `QueryEngine`. Manages API client, tool executor, permission engine.

### Main Flow

```
main() → parse_args() → Cli::Run → ReplLoop::run()
  → TerminalUi::enter() → event loop
    → crossterm key events → VimState::handle()
      → Insert mode: accumulates input, /command detection
      → Normal mode: navigation, scroll, search
    → On Enter: submit to Orchestrator
    → Orchestrator → QueryEngine.submit_streaming()
    → Engine events → UiState updates → TerminalUi::draw()
```

### Architecture Patterns

- **tokio async runtime** — Single multi-threaded runtime for all async operations.
- **Channel-based communication** — UI events, engine events, intents flow through mpsc channels.
- **`Arc<Mutex<UiState>>`** — Shared mutable state between render loop and event handlers.
- **Graceful shutdown** — Ctrl+C handler, shutdown oneshot channel.

## Current Status

### What Works

- Full CLI argument parsing with clap.
- REPL with Vim mode (normal/insert).
- Streaming API integration with real-time token display.
- Tool execution with permission dialogs.
- Session transcript persistence to `.coderex/transcripts/`.
- Settings loading from `~/.coderex/settings.json` and `.coderex/settings.json`.
- Tracing setup with JSON formatter option.
- Welcome screen with T-Rex ASCII art.
- Ctrl+C interrupt handling.

### What Is Stubbed/Partial

- **`Server` subcommand** — Starts a daemon but limited functionality.
- **`Config` subcommand** — Basic config editing, no validation.
- **Headless mode** — Not implemented; always starts TUI.
- **Non-interactive mode** — No `--prompt` or stdin pipe support.

## Gaps for Production

- **No daemon mode in REPL**: Cannot background the process.
  → **Suggestion**: Add a `--daemon` flag that forks the process (Unix) or uses `windows-service` crate to run as a Windows service. Store the PID in `~/.coderex/daemon.pid` for management.
- **No session restore on crash**: If process panics, unsaved state is lost.
  → **Suggestion**: Implement a `CrashRecovery` module that writes a snapshot of `UiState` (messages, input buffer) to `~/.coderex/recovery.json` every 30 seconds via a background task. Offer `/recover` on startup if the file exists.
- **Configuration reload**: Settings changes require restart.
  → **Suggestion**: Watch `~/.coderex/settings.json` and `.coderex/settings.json` for file modifications using `notify` crate. On change, re-parse and hot-swap the `ConfigService` behind an `Arc<RwLock<...>>`.
- **No multi-workspace support**: Only operates on single CWD.
  → **Suggestion**: Add a `--workspace` flag and `/workspace` command to switch between project directories. Maintain a separate `Orchestrator` instance per workspace with isolated transcripts and config.
- **Startup is slow**: Compiles many crates; no pre-built binary distribution.
  → **Suggestion**: Set up CI builds (GitHub Actions) for macOS, Linux, and Windows. Publish to GitHub Releases with a `self-update` feature using `self_update` crate. Cache compiled artifacts with `sccache`.
- **No piped input**: `echo "prompt" | coderex` does not work.
  → **Suggestion**: Detect `!std::io::stdin().is_terminal()` at startup. If piped, read all stdin, treat it as the initial prompt, run one turn, print the response, and exit. Skip TUI initialization.
- **API key from env only**: No keychain integration for `ANTHROPIC_API_KEY`.
  → **Suggestion**: Integrate with OS keychains via `keyring` crate. On first run, prompt for the API key and store it securely. Fallback to env var and config file.
- **No update check**: Does not notify about new versions.
  → **Suggestion**: On startup (once per day), fetch the latest GitHub release tag. Compare with the compiled `CARGO_PKG_VERSION`. If newer, show a one-line notification in the statusline or welcome screen.
- **Signal handling limited**: Only SIGINT; no SIGHUP, SIGTERM graceful shutdown.
  → **Suggestion**: Register handlers for SIGTERM and SIGHUP using `tokio::signal::unix`. On receipt, trigger the same graceful shutdown path as Ctrl+C: flush pending transcripts, save recovery snapshot, and exit.
