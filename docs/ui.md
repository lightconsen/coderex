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

The `coderex-ui` crate is fully implemented with no stubs or gaps for production.
