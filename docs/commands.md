# commands Module Inspection

## Goal

Implements the `/` slash command system that users type in the REPL. Provides registry, dispatch, and 14 command categories covering sessions, configuration, content management, diagnostics, Git integration, and debugging.

## Design

### Key Types

- **`CommandRegistry`** (`registry.rs`) — Maps command names to `CommandHandler` closures. Supports aliases.
- **`CommandContext`** (`registry.rs`) — Per-invocation context: session state, config service, UI state reference.
- **`CommandHandler`** (`registry.rs`) — Type alias for `fn(&mut CommandContext, &[String]) -> CommandResult`.
- **`SlashCommand`** (`registry.rs`) — Metadata: name, description, aliases, handler.
- **`CommandResult`** (`registry.rs`) — `Ok(String)` for success, `Err(String)` for failure.

### Command Categories (14 modules)

| Module | Commands | Purpose |
|--------|----------|---------|
| `session_commands` | `/clear`, `/compact`, `/exit`, `/save` | Session lifecycle |
| `config_commands` | `/config`, `/model`, `/mode` | Configuration |
| `stats_commands` | `/cost`, `/tokens` | Usage stats |
| `content_commands` | `/paste`, `/yank` | Clipboard/yank |
| `session_extension_commands` | `/load`, `/resume` | Session persistence |
| `config_extension_commands` | `/theme`, `/keymap` | UI config |
| `diagnostic_commands` | `/lint`, `/check` | Code diagnostics |
| `context_commands` | `/context`, `/files` | Context management |
| `integration_commands` | `/commit`, `/pr` | GitHub integration |
| `git_commands` | `/diff`, `/branch`, `/log` | Git operations |
| `debug_commands` | `/debug`, `/trace` | Debugging |
| `advanced_commands` | `/plan`, `/agent` | Advanced features |
| `misc_commands` | `/help`, `/version` | General |

### Architecture Patterns

- **Registry pattern** — Commands register themselves at startup via `register_all_commands()`.
- **Closure-based handlers** — Each command is a function pointer with shared `CommandContext`.
- **Modular organization** — Commands grouped by domain across 14 files.

## Current Status

### What Works

- Command registry with name-to-handler mapping.
- `/clear`, `/compact`, `/exit`, `/help`, `/version` fully functional.
- `/model` switches active model.
- `/cost` and `/tokens` report usage.
- Git integration commands (`/diff`, `/branch`, `/log`) work via git CLI.
- `/commit` command with conventional commit formatting.
- All commands have basic error handling.

### What Is Stubbed/Partial

- **`/plan` and `/agent`** — Invoke planning/agent flow but integration with `agents` crate is minimal.
- **`/pr`** — GitHub PR creation exists but may not handle auth properly.
- **`/lint` and `/check`** — Run external tools but output formatting is basic.
- **`/theme` and `/keymap`** — UI config commands may not persist changes.
- **Command completion/suggestions** — No fuzzy matching for command names.

## Gaps for Production

- **No command history**: Previous commands not searchable or recallable.
  → **Suggestion**: Persist command history to `~/.coderex/command_history.jsonl`. Support up-arrow recall in input mode and `/history` to list recent commands. Integrate with `InputBuffer` history.
- **No command validation**: Arguments are not validated against schemas before execution.
  → **Suggestion**: Add a `CommandArgs` trait with `validate(&[String]) -> Result<(), String>`. Each command defines its expected argument count/types. Validate before dispatching to the handler.
- **No undo for destructive commands**: `/clear` permanently wipes session without confirmation.
  → **Suggestion**: Tag commands with a `destructive: bool` flag in `SlashCommand`. Before executing destructive commands, show a `Confirm` overlay requiring `y/n` input.
- **GitHub integration lacks auth flow**: `/pr` assumes pre-configured credentials.
  → **Suggestion**: Detect missing `gh` CLI or `GITHUB_TOKEN` and prompt the user to run `gh auth login` or set the env var. Cache the auth check result per session.
- **No command macros**: Users cannot define custom command aliases or sequences.
  → **Suggestion**: Add a `/macro` command that records a sequence of commands (e.g., `/macro save "compact, save, exit"`). Store macros in `~/.coderex/macros.json` and expand them at parse time.
- **Command output not scrollable**: Long command output (e.g., `/log`) may overflow the TUI.
  → **Suggestion**: Treat command output as a temporary message in the message list, allowing the user to scroll through it. Add a `scrollable_output` flag to `CommandResult`.
- **No command piping**: Cannot chain commands (e.g., `/diff | /commit`).
  → **Suggestion**: Implement a simple pipe operator where the `Ok(String)` output of the first command becomes input/context for the second. Start with `/diff | /commit` as the primary use case.
