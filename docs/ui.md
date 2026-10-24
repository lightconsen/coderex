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
- **`mod_elements`** (`mod_elements.rs`) — Pure renderer for the UI tree a mod contributes (`UiElement` from `coderex_core::ui_protocol`). `render_mod_ui` returns styled `Line`s and `render_mod_ui_text` returns the plain block used by the transcript and headless output. Boxes nest with box-drawing borders and titles, `UiStyle` maps to the existing modifiers/colors, and buttons render as `[label]`. `render_mod_ui_with_focus` / `render_mod_ui_text_with_focus` mark one button as `> [label] <` with reversed emphasis. The renderer keeps no TUI state: focus is an argument, so a tree can be rendered after the mod runtime is gone. `render_mod_ui_with_hits` / `render_mod_ui_with_focus_and_hits` return the same lines plus `ButtonHit { id, row, col_start, col_end }` positions relative to the block; `button_at(rect, scroll_offset, column, row, hits)` is the pure click mapping (the focus marker is included in the rendered width, so it cannot shift a hit). Interactive key routing (Tab/Shift+Tab, Enter, Esc) and click routing live in `repl.rs`; headless output is not interactive.

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

### Mouse input

`TerminalUi::enter` enables mouse capture (`EnableMouseCapture`) and `exit`
disables it, so crossterm delivers wheel and button events. Wheel events scroll
the transcript in `repl.rs`.

Mod UI buttons are clickable. Before each draw the REPL passes a
`ModUiBlockSpec` for the newest block (message index, live `UiElement` tree and
focused id) to `TerminalUi::draw_with_mod_ui`. The transcript renderer
(`terminal.rs`) re-renders that block with
`render_mod_ui_with_focus_and_hits` at the message pane's actual content width,
draws those lines with the system-message prefix, and records a
`ModUiHitRegion` — the block's absolute `rect`, the number of block rows
clipped above it (`scroll_offset`), and its `ButtonHit`s — for the frame that
was just drawn. On `MouseEventKind::Down(MouseButton::Left)` the REPL asks
`mod_elements::button_at` to map the screen coordinates; a hit runs the same
activation path as `Enter`. Clicks are dropped while an overlay, a permission
confirmation or an MCP form owns the screen, and a stale spec is ignored when
its message content no longer matches (after `/clear`, rewind or resume). The
compact transcript (message pane under ten rows) truncates blocks to one line
and records no hit regions, so clicks are not routed there.

Keyboard focus is unchanged: `Tab`/`Shift+Tab`/`Enter`/`Esc` work in every
terminal, and they are the fallback where the terminal does not report mouse
events.

### Not handled

- **VS Code GPU acceleration.** Claude Code disables VS Code's GPU acceleration; CodeRex does not. There is no way to do it from inside the process — `terminal.integrated.gpuAcceleration` is a VS Code setting that requires editing `settings.json` and reloading the window. Shipping a flag that only pretends to change it would be worse than not shipping one, so VS Code is detected and nothing further is claimed.
- **`modifyOtherKeys`.** crossterm 0.28 does not decode xterm's `CSI 27 ; modifier ; code ~` form. We avoid the situation rather than mis-decode it: nothing here asks a terminal to enable modifyOtherKeys.
- **Terminals not in the table above** are `Unknown` and get the legacy encoding.

### Graphics protocol

**Not implemented — ⚪, deliberately.** The Kitty graphics protocol (with its image ids) is the one item in the UI checklist CodeRex does not take.

Reasoning: tool results can now carry `ImageBlock`s (MCP images, image reads), but the message list and its renderer are still text-only, so there is no place to draw them. Rendering inline images would mean a new dependency (`ratatui-image` plus an image decoder), a message/render variant, a capability handshake, and a fallback for every terminal that does not speak the protocol. The image data exists; the UI consumer does not. If images are ever needed, the honest path is `ratatui-image` gated on `TerminalProfile::kind` (kitty / WezTerm / Ghostty for Kitty graphics, iTerm2 for its own inline-image protocol, sixel elsewhere), degrading to the existing ASCII art.

No image capability detection was added either: a detection method with nothing to consume it would be exactly the kind of flag that does nothing.

## Architecture Patterns

- **ratatui widget composition** — Paragraph, Block, Layout with Constraint-based sizing.
- **Vim modal editing** — Normal, Insert, Visual and Visual-line modes. `VimState::handle_key` classifies normal/visual keys into `VimAction`s; `input::key_event_to_action` remains the classifier for non-Vim (insert) input.
  - **Motions and operators carry a count.** A digit prefix (`3w`, `5x`, `2>>`) is accumulated by `VimState` and carried inside the `VimAction` variant (`WordForward(3)`, `DeleteChar(5)`, `Indent(2)`), so the REPL applies it exactly as many times as asked. A leading `0` with no count is the `0` motion (jump to start); `0` after a digit is part of the number.
  - **Normal-mode operators** include `x` (delete char), `dd`/`D`, `dw`/`de`, `cc`/`cw`/`C` (change), `s`/`S` (substitute), `o`/`O` (open line below/above), `p`/`P` (paste), and `>`/`<` (indent/outdent the line).
  - **Visual mode** supports `h/j/k/l`, `w/b/e`, `0/$`, `gg/G` motions (with counts), `v`/`V` to switch selection kind, `y` to yank, `d`/`x` to delete, `c`/`s` to change the selection, `p` to paste over it, `>`/`<` to indent and leave, and `Esc` to leave. The statusline shows a `-- VISUAL --` / `-- VISUAL LINE --` indicator while a selection is anchored.
  - Selections operate on byte ranges while cursor movement and word motions step whole UTF-8 characters, so multi-byte input is never split. The selected range is highlighted with the theme's `selection_fg`/`selection_bg`, and the terminal cursor is positioned by display width (`unicode-width`) rather than byte offset.
  - **Single-line limitation.** The input buffer is one line, so the line-oriented Vim commands are applied to that single line: `j`/`k`, `gg`/`G`, and `o`/`O` cannot move between lines or open a new one below/above a real newline. `o`/`O` place the cursor at the end/start of the one line and enter Insert; `>`/`<` indent or outdent that line. This is the honest ceiling of a single-line editor and is noted rather than faked.
- **Virtualization** — `MessageView` computes visible messages based on per-message height (not just message count).
- **Word wrapping** — Custom `wrap_text()` with `unicode-width` for CJK support.
- **Overlay stack** — Modal dialogs can nest; inline palette is special-cased above statusline.
- **Testable terminal I/O** — The startup byte sequence is written by a plain function over `impl Write` (`enter_commands`), so the keyboard push can be asserted on rather than assumed.

## Current Status

The `coderex-ui` crate has no stubs. The integration gap below is real, and the Vim/Kittty boundaries in the checklist apply.

Known integration gap (owned by `crates/cli/src/repl.rs`, not this crate): the REPL matches key events inline rather than routing every key through `input::key_event_to_action`. The two safety properties that matter are nevertheless applied at the boundary: `repl.rs` calls `input::key_press` on every incoming event (release events are dropped), and `InputBuffer::apply(InsertChar)` rejects control characters, as does the REPL's Vim-insert path. `TerminalUi::profile()` is still not surfaced in the UI.

Tests: `cargo test -p coderex-ui` (248 passing).
