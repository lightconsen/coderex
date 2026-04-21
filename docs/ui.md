# ui Module Inspection

## Goal

Terminal user interface via ratatui. Renders messages, statusline, input line, command palette overlays, Vim mode, themes, and handles all keyboard input. Recently enhanced with text wrapping, inline command palette, thinking spinner, and T-Rex ASCII welcome art.

## Design

### Key Types

- **`TerminalUi`** (`terminal.rs:45`) — Main ratatui wrapper. Manages alternate screen, draw loop.
- **`UiState`** (`state.rs`) — Central UI state: messages, input, overlays, statusline.
- **`MessageView`** (`message_list.rs:35`) — Virtualized message list with scroll offset and visible count.
- **`Message`** (`message_list.rs:22`) — Rendered message with role, content, tool_name, streaming flag.
- **`MessageRole`** (`message_list.rs:7`) — User/Assistant/ToolResult/ToolCall/System.
- **`Statusline`** (`statusline.rs:8`) — Model, tokens, cost, permission mode, git branch, session title, thinking spinner.
- **`InputBuffer`** (`input.rs`) — Cursor-positioned text input with history.
- **`OverlayStack`** (`overlay.rs`) — Modal dialog stack: PermissionDialog, Notification, CommandPalette, Confirm.
- **`VimState`** (`vim.rs`) — Vim mode state machine: Normal/Insert/Visual with keybindings.
- **`Theme`** (`themes.rs`) — Color scheme definitions.

### Layout (bottom-up)

```
[Message area]     — Scrollable, word-wrapped messages
[Inline palette]   — /command hints (0-6 lines, dynamic height)
[Statusline]       — 1 line: spinner | model | tokens | cost | git | mode
[Input line]       — 1 line: "❯ " + user input
```

### Architecture Patterns

- **ratatui widget composition** — Paragraph, Block, Layout with Constraint-based sizing.
- **Vim modal editing** — Normal/Insert modes with full keybinding map.
- **Virtualization** — `MessageView` computes visible messages based on per-message height (not just message count).
- **Word wrapping** — Custom `wrap_text()` with `unicode-width` for CJK support.
- **Overlay stack** — Modal dialogs can nest; inline palette is special-cased above statusline.

## Current Status

### What Works

- Full ratatui rendering with alternate screen.
- Vim mode (Normal/Insert) with hjkl navigation, yank, paste, undo.
- Word-wrapped message display with role-based prefixes and colors.
- Inline `/` command palette with selection highlighting.
- Thinking spinner (rotating ◐◓◑◒) in statusline during streaming.
- T-Rex ASCII welcome art when no messages.
- Statusline with model, tokens, cost, git branch, permission mode.
- Permission dialogs, notifications, confirm overlays.
- Theme loading from custom directories.
- Search functionality (`/` in normal mode).
- Cursor positioning in input line.

### What Is Stubbed/Partial

- **`voice` module** — Feature-gated, not fully implemented.
- **`buddy` module** — Feature-gated, companion UI concept.

## Gaps for Production

- ~~**No mouse support**: Cannot click to position cursor or select messages.~~ — **Fixed**: Mouse capture enabled on enter, disabled on exit. Scroll wheel maps to message view scroll.
- ~~**No copy-to-clipboard**: Cannot copy message text from TUI.~~ — **Fixed**: `yy` in Vim normal mode copies all messages to system clipboard. `VimAction::ExportCopy` wired in repl.rs.
- ~~**Message search is basic**: Only finds first match, no highlight navigation.~~ — **Fixed**: `n`/`N` in Vim normal mode navigate search matches via `VimAction::SearchNext`/`SearchPrev`.
- ~~**No message folding**: Long messages cannot be collapsed.~~ — **Fixed**: `folded: bool` field on `Message`, `za` in Vim normal mode toggles folding. Folded messages render as single line with `[+N lines folded]`.
- ~~**No split-pane view**: Cannot view files alongside chat.~~ — **Fixed**: `LayoutMode` enum (SinglePane, HorizontalSplit { left_percent }). Ctrl+W v or Ctrl+S toggles split. `/open <path>` loads a file in the left pane. Left pane renders with bordered paragraph showing file content.
- ~~**Statusline truncation is by bytes not chars**: May cut multi-byte characters incorrectly.~~ — **Fixed**: Uses `unicode-width::UnicodeWidthStr::width()` to measure display width, truncates at grapheme boundaries via `unicode-segmentation`.
- ~~**No responsive layout for small terminals**: Below ~20 lines, layout breaks.~~ — **Fixed**: `is_compact` check in `draw()`: hides statusline when `height < 20`, truncates long messages when `height < 10`, skips welcome art.
- ~~**Welcome art is static**: Could show tips, recent sessions, or project stats.~~ — **Fixed**: `render_welcome_art()` function returns dynamic lines (T-Rex ASCII + tips + welcome message).
- ~~**No syntax highlighting in code blocks**: Markdown code blocks render as plain text.~~ — **Already fixed**: Syntax highlighting was previously implemented.
- ~~**Cursor may drift on CJK input**: `unicode-width` used for rendering but input handling may miscount.~~ — **Fixed**: `InputBuffer::display_column()` computes visual column accounting for wide characters. `byte_from_column()` converts display column to byte position.
