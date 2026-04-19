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
- **Theme hot-reload** — Themes load at startup but don't refresh without restart.
- **Export functionality** (`export.rs`) — Basic structure, not wired to commands.
- **Notification center** (`notification_center.rs`) — Types exist but not actively used.

## Gaps for Production

- **No mouse support**: Cannot click to position cursor or select messages.
  → **Suggestion**: Enable `crossterm::event::MouseEvent` capture. Map clicks to message selection, double-clicks to copy text, and wheel events to scroll. Add `mouse_enabled` config toggle.
- **No copy-to-clipboard**: Cannot copy message text from TUI.
  → **Suggestion**: In Normal mode, bind `y` (yank) to copy the current message to the system clipboard using the `arboard` crate. Support `yy` for the whole message, `yw` for a word.
- **Message search is basic**: Only finds first match, no highlight navigation.
  → **Suggestion**: Store search match positions as a `Vec<(line, column)>` in `UiState`. Bind `n`/`N` to jump to next/previous match. Highlight all matches in yellow, current match in red.
- **No message folding**: Long messages cannot be collapsed.
  → **Suggestion**: Add a `folded: bool` field to `Message`. In Normal mode, bind `za` to toggle folding. Render folded messages as a single line with `[+N lines]` indicator.
- **No split-pane view**: Cannot view files alongside chat.
  → **Suggestion**: Add a `LayoutMode` enum (SinglePane, HorizontalSplit). In split mode, render the left pane with file content (via Read tool) and right pane with chat. Bind `Ctrl-w v` to toggle.
- **Statusline truncation is by bytes not chars**: May cut multi-byte characters incorrectly.
  → **Suggestion**: Use `unicode-width::UnicodeWidthStr::width()` to compute display width before truncation. Truncate at grapheme boundaries using `unicode-segmentation`.
- **No responsive layout for small terminals**: Below ~20 lines, layout breaks.
  → **Suggestion**: Add a `compact_mode` that activates when `area.height < 20`. Hide the statusline, inline palette, and welcome art. Use single-line messages with truncation.
- **Welcome art is static**: Could show tips, recent sessions, or project stats.
  → **Suggestion**: Replace static ASCII with a dynamic widget that shows: (1) last 3 session names, (2) current project name, (3) a random tip from `tips.json`. Load tips asynchronously.
- **No syntax highlighting in code blocks**: Markdown code blocks render as plain text.
  → **Suggestion**: Detect ```language fences in messages. Use `syntect` or `tree-sitter` to tokenize code blocks and render them with `ratatui::text::Span` colorization based on the detected language.
- **Cursor may drift on CJK input**: `unicode-width` used for rendering but input handling may miscount.
  → **Suggestion**: Audit `InputBuffer` cursor movement to use `UnicodeWidthChar::width()` for all character indexing. Ensure `cursor_position` tracks display columns, not byte offsets. Add CJK input tests.
