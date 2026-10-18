# ui Module Inspection

## Goal

Terminal user interface via ratatui. Renders messages, statusline, input line, command palette overlays, Vim mode, themes, and handles all keyboard input. Identifies the host terminal and asks it for an unambiguous key encoding when it can.

## Design

### Key Types

- **`TerminalUi`** (`terminal.rs:55`) — Main ratatui wrapper. Manages alternate screen, the keyboard-protocol push/pop, and the draw loop.
- **`TerminalProfile`** (`terminal_profile.rs`) — What the environment says about the host terminal: `kind`, raw `TERM_PROGRAM`, and the chosen `keyboard_protocol`.
- **`TerminalKind`** (`terminal_profile.rs`) — Iterm2 / VsCode / Cursor / WindowsTerminal / Kitty / WezTerm / Ghostty / AppleTerminal / Vte / Unknown.
- **`KeyboardProtocol`** (`terminal_profile.rs`) — `Disambiguate` (push `DISAMBIGUATE_ESCAPE_CODES`) or `Legacy`.
- **`UiState`** (`state.rs`) — Central UI state: messages, input, overlays, statusline.
- **`MessageView`** (`message_list.rs:35`) — Virtualized message list with scroll offset and visible count.
- **`Message`** (`message_list.rs:22`) — Rendered message with role, content, tool_name, streaming flag.
- **`MessageRole`** (`message_list.rs:7`) — User/Assistant/ToolResult/ToolCall/System.
- **`Statusline`** (`statusline.rs:8`) — Model, tokens, cost, git branch, session title, thinking spinner. The permission mode is not here: it lives in `PermissionBar` (`permission_bar.rs`), drawn on its own line.
- **`InputBuffer`** (`input.rs`) — Cursor-positioned text input with history.
- **`OverlayStack`** (`overlay.rs`) — Modal dialog stack: PermissionDialog, Notification, CommandPalette, Confirm.
- **`VimState`** (`vim.rs`) — Vim mode state machine: Normal, Insert, Visual (`v`) and Visual-line (`V`). Selection anchors and the selected text live in `InputBuffer` (`input.rs`), which is the layer that knows the buffer contents.
- **`Theme`** (`themes.rs`) — Color scheme definitions.

### Layout (bottom-up)

```
[Message area]     — Scrollable, word-wrapped messages
[Inline palette]   — /command hints (0-6 lines, dynamic height)
[Notification bar] — 0-1 line, latest unseen notification
[Statusline]       — 1 line: spinner | model | tokens | cost | git
[Input line]       — 1 line: "❯ " + user input
[Permission bar]   — 0-1 line: permission mode
```

Every pane owns its own `Constraint`, including the ones that resolve to zero height. `Layout::split` does not compact zero-height rects away, so the pane indices (`PALETTE_IDX`…`PERMISSION_IDX` in `terminal.rs`) index the constraint order, never "which panes happen to be visible".

## Terminal adaptation

### What is handled

| Terminal | Identified | Key encoding |
|---|---|---|
| kitty (`KITTY_WINDOW_ID`) | yes | kitty keyboard protocol (disambiguate) |
| WezTerm (`WEZTERM_PANE`) | yes | kitty keyboard protocol (disambiguate) |
| Ghostty (`TERM_PROGRAM=ghostty`) | yes | kitty keyboard protocol (disambiguate) |
| iTerm2 (`TERM_PROGRAM=iTerm.app`) | yes | legacy |
| VS Code (`TERM_PROGRAM=vscode`) | yes | legacy |
| Cursor (`CURSOR_TRACE_ID`) | yes | legacy |
| Windows Terminal (`WT_SESSION`) | yes | legacy |
| Terminal.app (`TERM_PROGRAM=Apple_Terminal`) | yes | legacy |
| VTE family (`VTE_VERSION`) | yes | legacy |
| anything else | `Unknown` | legacy |

Identification reads the environment through a caller-supplied lookup (`TerminalProfile::detect`), so every rule above is covered by tests that describe a terminal without touching the process environment. Detection order matters and is pinned by tests: Cursor reports `TERM_PROGRAM=vscode`, and VS Code's integrated terminal on Windows can also carry `WT_SESSION`.

### Keyboard protocol — why this is a correctness item, not cosmetics

Under the legacy encoding `Ctrl+I` and `Tab`, `Ctrl+M` and `Enter`, and `Ctrl+[` and `Esc` are the same bytes on the wire; no decoder can separate them. The kitty keyboard protocol's `DISAMBIGUATE_ESCAPE_CODES` (`CSI > 1 u`) makes them distinct events, and crossterm decodes the resulting CSI-u sequences.

`TerminalUi::enter()` pushes the flag and `exit()` pops it (`terminal.rs`, `enter_commands` / `exit_commands`). The pop is conditional on the push having happened — popping a level that was never pushed would corrupt the stack for whatever runs after CodeRex.

The push is emitted **only** for kitty, WezTerm and Ghostty. VS Code, Cursor, iTerm2 and Windows Terminal are identified but deliberately left on the legacy encoding: their support for the protocol could not be verified from here, and guessing is worse than the status quo. Users on those terminals can opt in:

```
CODEREX_KEYBOARD_PROTOCOL=disambiguate   # force the kitty keyboard protocol
CODEREX_KEYBOARD_PROTOCOL=legacy         # force the terminal default
```

### Decoding is robust either way

`input::key_event_to_action` does not assume which encoding is in play:

- **Key releases are not keypresses.** Windows always reports releases, and the protocol reports them under `REPORT_EVENT_TYPES`; without the filter in `input::key_press`, every character is typed twice. Autorepeat is kept — it is a real keypress.
- **Control chords are case-insensitive.** Depending on the active enhancement flags, `Ctrl+Shift+C` is reported as `'c'` or `'C'` with CONTROL; both mean the same key.
- **Control characters are never inserted.** CSI-u can deliver `Char('\n')` where the legacy encoding delivered a named key. A single-line buffer must never take one.

### Not handled

- **VS Code GPU acceleration.** Claude Code disables VS Code's GPU acceleration; CodeRex does not. There is no way to do it from inside the process — `terminal.integrated.gpuAcceleration` is a VS Code setting that requires editing `settings.json` and reloading the window. Shipping a flag that only pretends to change it would be worse than not shipping one, so VS Code is detected and nothing further is claimed.
- **`modifyOtherKeys`.** crossterm 0.28 does not decode xterm's `CSI 27 ; modifier ; code ~` form. We avoid the situation rather than mis-decode it: nothing here asks a terminal to enable modifyOtherKeys.
- **Terminals not in the table above** are `Unknown` and get the legacy encoding.

### Graphics protocol

**Not implemented — ⚪, deliberately.** The Kitty graphics protocol (with its image ids) is the one item in the UI checklist CodeRex does not take.

Reasoning: the message list is text-only and tool results are text, so there is no content path that would produce an image to draw. Rendering inline images would mean a new dependency (`ratatui-image` plus an image decoder) that must not be added here, a new message variant, a capability handshake, and a fallback for every terminal that does not speak the protocol — in exchange for a feature with no producer and no consumer. If images are ever needed, the honest path is `ratatui-image` gated on `TerminalProfile::kind` (kitty / WezTerm / Ghostty for Kitty graphics, iTerm2 for its own inline-image protocol, sixel elsewhere), degrading to the existing ASCII art.

No image capability detection was added either: a detection method with nothing to consume it would be exactly the kind of flag that does nothing.

## Architecture Patterns

- **ratatui widget composition** — Paragraph, Block, Layout with Constraint-based sizing.
- **Vim modal editing** — Normal, Insert, Visual and Visual-line modes. `VimState::handle_key` classifies normal/visual keys into `VimAction`s; `input::key_event_to_action` remains the classifier for non-Vim (insert) input. Visual mode supports `h/j/k/l`, `w/b/e`, `0/$`, `gg/G` motions, `v`/`V` to switch selection kind, `y` to yank, `d`/`x` to delete, and `Esc` to leave. Selections operate on byte ranges while cursor movement and word motions step whole UTF-8 characters, so multi-byte input is never split. The selected range is highlighted with the theme's `selection_fg`/`selection_bg`, and the terminal cursor is positioned by display width (`unicode-width`) rather than byte offset.
- **Virtualization** — `MessageView` computes visible messages based on per-message height (not just message count).
- **Word wrapping** — Custom `wrap_text()` with `unicode-width` for CJK support.
- **Overlay stack** — Modal dialogs can nest; inline palette is special-cased above statusline.
- **Testable terminal I/O** — The startup byte sequence is written by a plain function over `impl Write` (`enter_commands`), so the keyboard push can be asserted on rather than assumed.

## Current Status

The `coderex-ui` crate has no stubs. The integration gap below is real, and the Vim/Kittty boundaries in the checklist apply.

Known integration gap (owned by `crates/cli/src/repl.rs`, not this crate): the REPL matches key events inline rather than routing every key through `input::key_event_to_action`. The two safety properties that matter are nevertheless applied at the boundary: `repl.rs` calls `input::key_press` on every incoming event (release events are dropped), and `InputBuffer::apply(InsertChar)` rejects control characters, as does the REPL's Vim-insert path. `TerminalUi::profile()` is still not surfaced in the UI.

Tests: `cargo test -p coderex-ui` (174 passing).
