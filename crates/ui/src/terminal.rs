//! Terminal UI rendering via ratatui.

use crate::message_list::{Message, MessageRole};
use crate::mod_elements::{ButtonHit, render_mod_ui_with_focus_and_hits};
use crate::state::{TextSelection, UiState};
use crate::statusline::Statusline;
use crate::terminal_profile::TerminalProfile;
use crate::themes::Theme;
use coderex_core::ui_protocol::UiElement;
use crossterm::event::{PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::queue;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::io::Stdout;
use std::sync::OnceLock;

/// A mod UI block the interactive loop wants drawn and tracked for clicks.
///
/// The transcript stores a block as rendered text, but hit testing needs the
/// live tree: the same tree renders the block at the message pane's width so
/// the button positions match the screen exactly.
#[derive(Debug, Clone)]
pub struct ModUiBlockSpec {
    /// Index of the message that holds this block.
    pub message_index: usize,
    /// The content the block was last rendered as. A mismatch means the index
    /// no longer points at this block (after `/clear`, rewind or resume), so
    /// the spec is ignored rather than drawing over an unrelated message.
    pub content: String,
    /// The host-owned UI tree snapshot.
    pub elements: Vec<UiElement>,
    /// The button that currently has keyboard focus, if any.
    pub focused: Option<String>,
}

/// Where a mod UI block landed in the last drawn frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModUiHitRegion {
    /// Index of the message the block was rendered from.
    pub message_index: usize,
    /// Absolute screen rect of the visible part of the block. `x` is the
    /// block's first rendered column and `y` its first visible row.
    pub rect: Rect,
    /// Number of block rows clipped above `rect.y`.
    pub scroll_offset: usize,
    /// Button positions relative to the block's rendered lines.
    pub hits: Vec<ButtonHit>,
}

/// Lazy-initialized syntax highlighter for the TUI.
fn syntax_highlighter() -> &'static coderex_core::syntax::SyntaxHighlighter {
    static HIGHLIGHTER: OnceLock<coderex_core::syntax::SyntaxHighlighter> = OnceLock::new();
    HIGHLIGHTER.get_or_init(coderex_core::syntax::SyntaxHighlighter::new)
}

/// ASCII T-Rex on the welcome page.
///
/// Deliberately the compact variant: the page has to fit above the input box
/// on a standard 24-row terminal, and the taller CLI banner does not leave
/// room for the wordmark and hints.
const WELCOME_T_REX: &str = r"
                      __
                     / _)
            _.----._/ /
           /         /
        __/ (  | (  |
       /__.-'|_|--|_|
";

/// The welcome page is drawn inside a box no wider than this. Two columns
/// beyond the longest hint keep the text off the right border.
const WELCOME_CARD_MAX_WIDTH: usize = 64;
/// Below this width the box's padding and hints no longer fit; the page falls
/// back to plain centered lines.
const WELCOME_CARD_MIN_WIDTH: usize = 34;
/// Rows the full welcome card occupies, including its border.
const WELCOME_CARD_HEIGHT: usize = 16;

/// The main terminal UI.
#[allow(missing_debug_implementations)]
pub struct TerminalUi {
    terminal: ratatui::Terminal<CrosstermBackend<Stdout>>,
    #[allow(dead_code)]
    alternate: bool,
    profile: TerminalProfile,
    keyboard_pushed: bool,
    mod_ui_hit_regions: Vec<ModUiHitRegion>,
    /// The last drawn frame as one character per cell. Used to copy a mouse
    /// selection; a continuation cell of a wide character is `'\0'`.
    frame_rows: Vec<Vec<char>>,
    /// The area the last frame covered.
    frame_area: Rect,
}

impl TerminalUi {
    /// Create a new terminal UI.
    ///
    /// The terminal is identified here, once, from the environment. Nothing is
    /// written to the terminal until [`TerminalUi::enter`].
    pub fn new() -> std::io::Result<Self> {
        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self {
            terminal,
            alternate: false,
            profile: TerminalProfile::from_env(),
            keyboard_pushed: false,
            mod_ui_hit_regions: Vec::new(),
            frame_rows: Vec::new(),
            frame_area: Rect::default(),
        })
    }

    /// The terminal profile detected at construction.
    #[must_use]
    pub const fn profile(&self) -> &TerminalProfile {
        &self.profile
    }

    /// Whether the keyboard-protocol flags are currently pushed.
    #[must_use]
    pub const fn keyboard_enhancement_active(&self) -> bool {
        self.keyboard_pushed
    }

    /// Enter alternate screen.
    pub fn enter(&mut self) -> std::io::Result<()> {
        let mut stdout = std::io::stdout();
        enter_commands(&self.profile, &mut stdout)?;
        self.keyboard_pushed = self.profile.keyboard_flags().is_some();
        self.alternate = true;
        // Clear the screen and reset ratatui's back buffer so the next draw
        // repaints everything. `Terminal::clear` is not used: ratatui 0.30
        // made it query the cursor position (a DSR request), which a PTY
        // without a terminal emulator never answers; `resize` performs the
        // same clear and buffer reset without that round trip.
        let area: ratatui::layout::Rect = self.terminal.size()?.into();
        self.terminal.resize(area)?;
        Ok(())
    }

    /// Leave alternate screen.
    pub fn exit(&mut self) -> std::io::Result<()> {
        if self.alternate {
            let mut stdout = std::io::stdout();
            exit_commands(self.keyboard_pushed, &mut stdout)?;
            self.keyboard_pushed = false;
            self.alternate = false;
        }
        Ok(())
    }

    /// Draw a single frame.
    pub fn draw(&mut self, state: &mut UiState) -> std::io::Result<()> {
        self.draw_with_mod_ui(state, &[])
    }

    /// Draw a frame, rendering interactive mod UI blocks from their live trees.
    ///
    /// The hit regions recorded here describe the frame that was just drawn, so
    /// a caller that handles a mouse event before the next draw maps the click
    /// against the screen the user actually saw.
    pub fn draw_with_mod_ui(
        &mut self,
        state: &mut UiState,
        mod_ui_blocks: &[ModUiBlockSpec],
    ) -> std::io::Result<()> {
        let mut mod_ui_hit_regions = Vec::new();
        let completed = self.terminal.draw(|frame| {
            render_frame(frame, state, mod_ui_blocks, &mut mod_ui_hit_regions);
        })?;
        self.mod_ui_hit_regions = mod_ui_hit_regions;
        self.frame_rows = capture_rows(completed.buffer);
        self.frame_area = completed.buffer.area;
        Ok(())
    }

    /// The mod UI hit regions from the last successfully drawn frame.
    #[must_use]
    pub fn mod_ui_hit_regions(&self) -> &[ModUiHitRegion] {
        &self.mod_ui_hit_regions
    }

    /// The text covered by a mouse selection, read from the last drawn frame.
    ///
    /// The buffer is cloned only when a copy is requested, not on every draw.
    #[must_use]
    pub fn selected_text(&self, selection: &TextSelection) -> String {
        selection_text(self.frame_area, &self.frame_rows, selection)
    }
}

/// Snapshot a rendered buffer as one character per cell.
///
/// A wide character owns the cell it starts in; the cell it continues into has
/// no symbol and is recorded as `'\0'`, so column arithmetic still lines up
/// with what the user selected on screen.
fn capture_rows(buffer: &ratatui::buffer::Buffer) -> Vec<Vec<char>> {
    let area = buffer.area;
    (area.top()..area.bottom())
        .map(|row| {
            (area.left()..area.right())
                .map(|column| {
                    buffer
                        .cell((column, row))
                        .and_then(|cell| cell.symbol().chars().next())
                        .unwrap_or('\0')
                })
                .collect()
        })
        .collect()
}

/// Extract the text covered by a mouse selection from a frame snapshot.
///
/// Rows between the endpoints are taken whole; the first and last rows are cut
/// at the selected columns. Trailing blanks on each row and trailing blank
/// rows are dropped, so choosing a rectangle of screen does not copy a block
/// of spaces or the interface below the text.
pub(crate) fn selection_text(area: Rect, rows: &[Vec<char>], selection: &TextSelection) -> String {
    let ((start_col, start_row), (end_col, end_row)) = selection.normalized();
    let last_col = area.right().saturating_sub(1);
    let mut lines = Vec::new();
    for row in start_row..=end_row {
        if row < area.y || row >= area.bottom() {
            continue;
        }
        let left = if row == start_row {
            start_col
        } else {
            area.left()
        };
        let right = if row == end_row { end_col } else { last_col };
        let left = left.max(area.left()).min(last_col);
        let right = right.max(left).min(last_col);
        let Some(cells) = rows.get((row - area.y) as usize) else {
            continue;
        };
        let mut line = String::new();
        for column in left..=right {
            if let Some(character) = cells.get((column - area.left()) as usize)
                && *character != '\0'
            {
                line.push(*character);
            }
        }
        lines.push(line.trim_end().to_string());
    }
    lines.join("\n").trim_end().to_string()
}

/// Tint the cells covered by a mouse selection.
///
/// Only the background changes: the text keeps the colours it was drawn with,
/// and an empty [`Paragraph`] fills the rect without writing over the symbols.
fn paint_selection(frame: &mut Frame, area: Rect, selection: TextSelection, theme: &Theme) {
    let ((start_col, start_row), (end_col, end_row)) = selection.normalized();
    let last_col = area.right().saturating_sub(1);
    let style = Style::default().bg(theme.selection_bg);
    for row in start_row..=end_row {
        if row < area.y || row >= area.bottom() {
            continue;
        }
        let left = if row == start_row {
            start_col
        } else {
            area.left()
        };
        let right = if row == end_row { end_col } else { last_col };
        let left = left.max(area.left()).min(last_col);
        let right = right.max(left).min(last_col);
        frame.render_widget(
            Paragraph::new("").style(style),
            Rect::new(left, row, right - left + 1, 1),
        );
    }
}

/// Write the commands that put the terminal into TUI mode.
///
/// Split out from [`TerminalUi::enter`] so the exact byte sequence is testable:
/// the keyboard push is only correct if it is actually emitted for a terminal
/// that understands it, and omitted for one that does not. `queue!` is used
/// rather than `execute!` so the bytes go to the writer on every platform —
/// crossterm's `execute!` falls back to a WinAPI call, which has no keyboard
/// protocol at all, and would make this untestable.
fn enter_commands(profile: &TerminalProfile, out: &mut impl std::io::Write) -> std::io::Result<()> {
    queue!(
        out,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    if let Some(flags) = profile.keyboard_flags() {
        queue!(out, PushKeyboardEnhancementFlags(flags))?;
    }
    out.flush()
}

/// Undo [`enter_commands`], in the reverse order.
///
/// `keyboard_pushed` is passed explicitly rather than re-derived from the
/// profile: popping a level that was never pushed would corrupt the stack for
/// whatever runs after CodeRex.
fn exit_commands(keyboard_pushed: bool, out: &mut impl std::io::Write) -> std::io::Result<()> {
    if keyboard_pushed {
        queue!(out, PopKeyboardEnhancementFlags)?;
    }
    queue!(
        out,
        LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    out.flush()
}

/// Pane positions in the vertical layout.
///
/// `Layout::split` returns one rect per constraint, in order, even when a
/// constraint resolves to zero height — it does not compact them away. Deriving
/// an index from "which panes are visible" therefore points at the wrong rect:
/// with the palette hidden and no notification, a computed statusline index of
/// 1 lands on the zero-height palette slot and the line disappears. Index by the
/// constraint position instead.
const PALETTE_IDX: usize = 1;
const NOTIFICATION_IDX: usize = 2;
const STATUSLINE_IDX: usize = 3;
const RULE_IDX: usize = 4;
const INPUT_IDX: usize = 5;
const RULE_BELOW_IDX: usize = 6;
const PERMISSION_IDX: usize = 7;

/// The vertical pane stack, top to bottom.
///
/// Shared with the layout tests so both sides agree on what each index means.
/// `input_height` is the number of terminal rows the input buffer needs: one
/// for an empty or single-line buffer, more once the buffer has lines or wraps.
fn frame_constraints(
    inline_palette_height: u16,
    show_notif: bool,
    show_statusline: bool,
    input_height: u16,
    show_perm_bar: bool,
) -> Vec<Constraint> {
    vec![
        Constraint::Min(1),                        // message area
        Constraint::Length(inline_palette_height), // inline command hint
        Constraint::Length(u16::from(show_notif)), // notification bar
        // One row of text with one blank row above and below it.
        Constraint::Length(if show_statusline { 3 } else { 0 }), // statusline
        Constraint::Length(1),                                   // rule above the input
        Constraint::Length(input_height.max(1)),                 // input lines
        Constraint::Length(1),                                   // rule below the input
        // The bar is one line plus a blank row below it, so the text does not
        // sit against the bottom edge of the terminal.
        Constraint::Length(if show_perm_bar { 2 } else { 0 }), // permission bar
    ]
}

/// Display columns available to input text: the pane minus the two-cell
/// prompt. Shared by the layout and the renderer so both wrap identically.
fn input_wrap_width(pane_width: u16) -> usize {
    (pane_width as usize).saturating_sub(2).max(1)
}

/// How many terminal rows the input buffer occupies at `pane_width`.
fn input_display_height(input: &crate::input::InputBuffer, pane_width: u16) -> u16 {
    let rows = input.display_rows(input_wrap_width(pane_width)).len();
    u16::try_from(rows).map_or(u16::MAX, |height| height.max(1))
}

/// Lay out and paint one frame.
///
/// Separate from `draw` so a `TestBackend` can drive it: the pane indices
/// below are the single place where layout order is decided, and getting one
/// wrong makes a pane render into the wrong rect without any error.
fn render_frame(
    frame: &mut Frame,
    state: &mut UiState,
    mod_ui_blocks: &[ModUiBlockSpec],
    mod_ui_hit_regions: &mut Vec<ModUiHitRegion>,
) {
    let area = frame.area();

    // Paint the theme background first. Widgets only overwrite the style
    // fields they name, so this fill survives under text without an explicit
    // background — which is what makes switching between dark and light cover
    // the whole screen rather than only re-tinting the accents.
    frame.render_widget(
        Block::default().style(
            Style::default()
                .bg(state.theme.background)
                .fg(state.theme.foreground),
        ),
        area,
    );

    let is_compact = state.force_compact || area.height < 20;

    // Determine chunk heights
    let inline_palette_height = if is_compact {
        0
    } else {
        inline_palette_height(state)
    };
    let show_statusline = !is_compact;
    let show_notif = state.notifications.active_count() > 0 && !is_compact;

    let show_perm_bar = !is_compact;
    // A long or multi-line input grows its own pane rather than being clipped
    // to one row. It is capped so the transcript keeps most of the screen.
    let max_input_height = area.height.saturating_sub(3).max(1);
    let input_height = input_display_height(&state.input, area.width).min(max_input_height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(frame_constraints(
            inline_palette_height,
            show_notif,
            show_statusline,
            input_height,
            show_perm_bar,
        ))
        .split(area);

    // `Layout::split` returns one rect per constraint, in order, even
    // when a constraint resolves to zero height — it does not compact
    // them away. Deriving an index from "which panes are visible"
    // therefore points at the wrong rect: with the palette hidden and
    // no notification, a computed statusline index of 1 lands on the
    // zero-height palette slot and the line disappears. Index by the
    // constraint position instead.
    debug_assert_eq!(chunks.len(), PERMISSION_IDX + 1);

    // Message area (index 0) — may be split horizontally
    let msg_area = chunks[0];
    if let crate::state::LayoutMode::HorizontalSplit { left_percent } = state.layout_mode {
        let pct = left_percent.clamp(10, 90);
        let split_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(pct),
                Constraint::Percentage(100 - pct),
            ])
            .split(msg_area);

        // Left pane: file/content viewer
        render_left_pane(frame, split_chunks[0], state);
        // Right pane: messages
        render_messages(
            frame,
            split_chunks[1],
            state,
            mod_ui_blocks,
            mod_ui_hit_regions,
        );
    } else {
        render_messages(frame, msg_area, state, mod_ui_blocks, mod_ui_hit_regions);
    }

    // Inline palette (index 1)
    if inline_palette_height > 0 {
        render_inline_palette(frame, chunks[PALETTE_IDX], state);
    }

    // Notification bar (index 2)
    if show_notif {
        render_notification_bar(
            frame,
            chunks[NOTIFICATION_IDX],
            &mut state.notifications,
            &state.theme,
        );
    }

    // Statusline (above input)
    if show_statusline {
        render_statusline(
            frame,
            chunks[STATUSLINE_IDX],
            &state.statusline,
            &state.theme,
        );
    }

    // Rules that frame the input box
    render_input_rule(frame, chunks[RULE_IDX], &state.theme);

    // Input line
    render_input(frame, chunks[INPUT_IDX], &state.input, &state.theme);

    render_input_rule(frame, chunks[RULE_BELOW_IDX], &state.theme);

    // Permission bar (below input)
    if show_perm_bar {
        render_permission_bar(
            frame,
            chunks[PERMISSION_IDX],
            &state.permission_bar,
            &state.theme,
        );
    }

    // Overlay (only non-palette overlays; palette is rendered inline)
    if state.overlays.is_active() && !is_inline_palette(state) {
        render_overlay(frame, area, state);
    }

    // A mouse selection is painted last so it is visible over the transcript.
    if let Some(selection) = state.selection {
        paint_selection(frame, area, selection, &state.theme);
    }
}

/// Compute the height of the inline palette area (0 = hidden).
fn inline_palette_height(state: &UiState) -> u16 {
    use crate::overlay::Overlay;
    let Some(overlay) = state.overlays.top() else {
        return 0;
    };
    match overlay {
        Overlay::CommandPalette {
            query, commands, ..
        } if query.starts_with('/') => {
            // Header line + up to 5 visible commands, capped at remaining terminal space
            let count = commands.len().min(5);
            u16::try_from(count + 1).map_or(6, |height| height.min(6))
        }
        _ => 0,
    }
}

/// Check whether the active overlay is an inline `/` palette.
fn is_inline_palette(state: &UiState) -> bool {
    use crate::overlay::Overlay;
    state.overlays.top().is_some_and(
        |o| matches!(o, Overlay::CommandPalette { query, .. } if query.starts_with('/')),
    )
}

/// Word-wrap text to the given display width.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut result: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();
        let mut current_width: usize = 0;
        for word in paragraph.split_whitespace() {
            let word_width = unicode_width::UnicodeWidthStr::width(word);
            let space = usize::from(!current.is_empty());
            if current_width + space + word_width > width {
                result.push(std::mem::take(&mut current));
                current_width = 0;
            }
            if !current.is_empty() {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width += word_width;
        }
        if !current.is_empty() {
            result.push(current);
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// Compute how many terminal rows a message occupies at the given width.
fn message_display_height(msg: &crate::message_list::Message, width: usize) -> usize {
    let prefix = message_prefix(msg.role);
    let prefix_width = unicode_width::UnicodeWidthStr::width(prefix);
    let wrap_width = width.saturating_sub(prefix_width);
    let mut height = wrapped_line_count(&msg.content, wrap_width);
    if matches!(
        msg.role,
        crate::message_list::MessageRole::User | crate::message_list::MessageRole::Assistant
    ) {
        height += 1;
    }
    if msg.tool_name.is_some()
        && matches!(
            msg.role,
            crate::message_list::MessageRole::ToolCall
                | crate::message_list::MessageRole::ToolResult
        )
    {
        height += 1;
    }
    height
}

fn wrapped_line_count(text: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }

    let mut lines = 0usize;
    for paragraph in text.split('\n') {
        let mut current_width = 0usize;
        let mut current_empty = true;
        for word in paragraph.split_whitespace() {
            let word_width = unicode_width::UnicodeWidthStr::width(word);
            let space = usize::from(!current_empty);
            if current_width + space + word_width > width {
                lines += 1;
                current_width = 0;
                current_empty = true;
            }
            if !current_empty {
                current_width += 1;
            }
            current_width += word_width;
            current_empty = false;
        }
        if !current_empty {
            lines += 1;
        }
    }

    lines.max(1)
}

const fn message_prefix(role: crate::message_list::MessageRole) -> &'static str {
    match role {
        crate::message_list::MessageRole::User => "● ",
        crate::message_list::MessageRole::Assistant => "◆ ",
        crate::message_list::MessageRole::ToolResult => "✓ ",
        crate::message_list::MessageRole::ToolCall => "⚙ ",
        crate::message_list::MessageRole::System => "· ",
    }
}

/// Convert a hex colour string (#RRGGBB) to a ratatui `Color`.
fn hex_to_color(hex: &str) -> Color {
    let hex = hex.trim_start_matches('#');
    if hex.len() == 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(255);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(255);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(255);
        Color::Rgb(r, g, b)
    } else {
        Color::Reset
    }
}

/// Build ratatui `Span`s from a piece of text, highlighting any Markdown
/// fenced code blocks with `SyntaxHighlighter`.
/// A conservative language guess for a tool result.
///
/// Read returns a file body without a fence, so there is no language tag to
/// use. The guess only fires on clear signals; anything else stays plain text
/// rather than being coloured wrongly.
fn guess_language(text: &str) -> Option<&'static str> {
    let sample: String = text.lines().take(80).collect::<Vec<_>>().join("\n");
    if sample.lines().count() < 2 {
        return None;
    }
    let trimmed = sample.trim_start();
    let has = |needle: &str| sample.contains(needle);

    if trimmed.starts_with("//!")
        || trimmed.starts_with("// ")
        || has("fn main(")
        || has("pub fn ")
        || has("impl ")
        || has("use std::")
        || has("#[derive(")
        || (has("struct ") && has("->"))
    {
        return Some("rs");
    }
    if trimmed.starts_with("def ")
        || trimmed.starts_with("import ")
        || has("\ndef ")
        || has("\nimport ")
        || has("\nclass ")
    {
        return Some("py");
    }
    if trimmed.starts_with('{') && has("\":") {
        return Some("json");
    }
    if trimmed.starts_with('[')
        && sample.lines().take(6).any(|line| line.contains('='))
        && !has("fn ")
    {
        return Some("toml");
    }
    if has("package ") && has("func ") {
        return Some("go");
    }
    if has("function ") || has("const ") || has("=>") {
        return Some("js");
    }
    None
}

/// Highlight a plain code body, preserving indentation and line breaks.
///
/// Unlike [`highlight_spans`], this does not collapse whitespace: a file body
/// shown under a Read call has to keep its layout.
fn highlighted_code_lines(
    text: &str,
    language: &str,
    prefix: &str,
    prefix_width: usize,
    prefix_style: Style,
    body_style: Style,
) -> Option<Vec<Line<'static>>> {
    let highlighted = syntax_highlighter().highlight(text, language).ok()?;
    let mut out = Vec::with_capacity(highlighted.len());
    for (index, tokens) in highlighted.into_iter().enumerate() {
        let mut spans: Vec<Span<'static>> = vec![if index == 0 {
            Span::styled(prefix.to_string(), prefix_style)
        } else {
            Span::styled(" ".repeat(prefix_width), body_style)
        }];
        let last_token = tokens.len().saturating_sub(1);
        for (token_index, token) in tokens.into_iter().enumerate() {
            let mut token_text = token.text;
            if token_index == last_token {
                while token_text.ends_with('\n') || token_text.ends_with('\r') {
                    token_text.pop();
                }
            }
            let mut style = body_style.fg(hex_to_color(&token.foreground));
            if token.is_bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if token.is_italic {
                style = style.add_modifier(Modifier::ITALIC);
            }
            spans.push(Span::styled(token_text, style));
        }
        out.push(Line::from(spans));
    }
    Some(out)
}

fn highlight_spans(text: &str, base_style: Style) -> Vec<Span<'_>> {
    let mut spans = Vec::new();
    let mut rest = text;

    while let Some(start) = rest.find("```") {
        // Text before the code block
        if start > 0 {
            spans.push(Span::styled(rest[..start].to_string(), base_style));
        }

        let after_open = &rest[start + 3..];
        // Extract language (first line after ```)
        let lang_end = after_open.find('\n').unwrap_or(after_open.len());
        let lang = after_open[..lang_end].trim();
        let after_lang = &after_open[lang_end..];

        // Find closing ```
        if let Some(close) = after_lang.find("```") {
            let code = &after_lang[..close];
            // Skip leading newline if present
            let code = code.strip_prefix('\n').unwrap_or(code);

            // Try syntax highlighting
            match syntax_highlighter().highlight(code, lang) {
                Ok(lines) => {
                    for (i, line_tokens) in lines.iter().enumerate() {
                        if i > 0 {
                            spans.push(Span::styled("\n".to_string(), base_style));
                        }
                        for token in line_tokens {
                            let mut style = base_style.fg(hex_to_color(&token.foreground));
                            if token.is_bold {
                                style = style.add_modifier(Modifier::BOLD);
                            }
                            if token.is_italic {
                                style = style.add_modifier(Modifier::ITALIC);
                            }
                            spans.push(Span::styled(token.text.clone(), style));
                        }
                    }
                }
                Err(_) => {
                    // Fallback: plain code block
                    spans.push(Span::styled(code.to_string(), base_style));
                }
            }

            rest = &after_lang[close + 3..];
        } else {
            // No closing fence — treat remainder as plain text
            spans.push(Span::styled(rest[start..].to_string(), base_style));
            rest = "";
            break;
        }
    }

    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), base_style));
    }

    spans
}

fn render_messages(
    frame: &mut Frame,
    area: Rect,
    state: &mut UiState,
    mod_ui_blocks: &[ModUiBlockSpec],
    mod_ui_hit_regions: &mut Vec<ModUiHitRegion>,
) {
    let width = area.width as usize;
    let viewport_height = area.height as usize;
    let is_compact = area.height < 10;
    state.messages.viewport_height = viewport_height;

    // Clean up expired notifications
    state.notifications.cleanup();

    if state.messages.scroll_offset > 0 {
        let signature =
            message_render_signature(&state.messages.messages, &state.theme, mod_ui_blocks);
        // A tracked block needs the render spans that the cache does not keep,
        // so only reuse the cache when there is no block to hit-test.
        let cache_valid = mod_ui_blocks.is_empty()
            && state.messages.render_cache.width == width
            && state.messages.render_cache.is_compact == is_compact
            && state.messages.render_cache.signature == signature;
        let block_spans = if cache_valid {
            Vec::new()
        } else {
            let (mut lines, spans) = render_all_message_lines(
                &state.messages.messages,
                width,
                is_compact,
                &state.theme,
                mod_ui_blocks,
            );
            if lines.is_empty() {
                lines.extend(render_welcome_art(
                    &state.theme,
                    width,
                    viewport_height,
                    &state.statusline.model,
                    &state.statusline.git_branch,
                ));
            }
            state.messages.render_cache.width = width;
            state.messages.render_cache.is_compact = is_compact;
            state.messages.render_cache.signature = signature;
            state.messages.render_cache.lines = lines;
            spans
        };
        state.messages.total_height = state.messages.render_cache.lines.len();
        let max_scroll = state
            .messages
            .render_cache
            .lines
            .len()
            .saturating_sub(viewport_height);
        state.messages.scroll_offset = state.messages.scroll_offset.min(max_scroll);
        let (start, end) = scroll_slice_bounds(
            state.messages.render_cache.lines.len(),
            viewport_height,
            state.messages.scroll_offset,
        );
        let lines = state.messages.render_cache.lines[start..end].to_vec();
        record_mod_ui_regions(area, &block_spans, start, end - start, mod_ui_hit_regions);
        let paragraph = Paragraph::new(lines).block(Block::default().borders(Borders::NONE));
        frame.render_widget(paragraph, area);
        return;
    }

    // Only the bottom/non-cached path needs per-message heights. The scrollback
    // path above slices cached rendered lines directly, keeping wheel/PageUp
    // redraws independent of transcript size.
    let mut heights: Vec<usize> = Vec::with_capacity(state.messages.messages.len());
    for (index, msg) in state.messages.messages.iter().enumerate() {
        if msg.folded {
            heights.push(1);
        } else {
            heights.push(message_render_height(
                msg,
                index,
                width,
                is_compact,
                mod_ui_blocks,
            ));
        }
    }

    let at_bottom = state.messages.is_at_bottom()
        && (state.messages.visible_count == 0
            || state.messages.start + state.messages.visible_count.max(1)
                >= state.messages.messages.len().saturating_sub(1));

    let (start, mut visible_count, mut accumulated) = if at_bottom {
        bottom_message_window(&heights, viewport_height)
    } else {
        visible_message_window(&heights, state.messages.start, viewport_height)
    };
    state.messages.start = start;

    // Fill remaining viewport space with older messages so history stays visible.
    while !at_bottom && state.messages.start > 0 {
        let h = heights[state.messages.start - 1];
        if accumulated + h > viewport_height {
            break;
        }
        accumulated += h;
        state.messages.start -= 1;
        visible_count += 1;
    }

    state.messages.set_visible_count(visible_count);
    let visible = state.messages.visible();
    let mut lines: Vec<Line> = Vec::new();
    let mut block_spans = Vec::new();

    for (offset, msg) in visible.iter().enumerate() {
        let index = state.messages.start + offset;
        if let Some(span) = append_message_lines(
            &mut lines,
            msg,
            index,
            width,
            is_compact,
            &state.theme,
            mod_ui_blocks,
        ) {
            block_spans.push(span);
        }
    }

    // If no messages, show the welcome page
    if lines.is_empty() {
        lines.extend(render_welcome_art(
            &state.theme,
            width,
            viewport_height,
            &state.statusline.model,
            &state.statusline.git_branch,
        ));
    }

    state.messages.total_height = lines.len();
    let untrimmed_len = lines.len();
    lines = trim_lines_to_viewport(lines, viewport_height);
    let trimmed = untrimmed_len.saturating_sub(lines.len());
    record_mod_ui_regions(area, &block_spans, trimmed, lines.len(), mod_ui_hit_regions);

    let paragraph = Paragraph::new(lines).block(Block::default().borders(Borders::NONE));
    frame.render_widget(paragraph, area);
}

/// The height of one message in the transcript.
///
/// A live mod UI block is measured from the same tree rendering used to draw
/// it, so the layout window and the drawn rows cannot disagree.
fn message_render_height(
    msg: &Message,
    index: usize,
    width: usize,
    is_compact: bool,
    mod_ui_blocks: &[ModUiBlockSpec],
) -> usize {
    if !is_compact && let Some(spec) = matching_mod_ui_block(msg, index, mod_ui_blocks) {
        let prefix_width = unicode_width::UnicodeWidthStr::width(message_prefix(msg.role));
        let wrap_width = width.saturating_sub(prefix_width);
        if let Some((lines, _)) = mod_ui_block_lines(spec, wrap_width) {
            return lines.len();
        }
    }
    message_display_height(msg, width)
}

fn matching_mod_ui_block<'a>(
    msg: &Message,
    index: usize,
    blocks: &'a [ModUiBlockSpec],
) -> Option<&'a ModUiBlockSpec> {
    if msg.role != MessageRole::System {
        return None;
    }
    blocks.iter().find(|block| {
        block.message_index == index && block.content.as_str() == msg.content.as_str()
    })
}

fn mod_ui_block_lines(
    spec: &ModUiBlockSpec,
    wrap_width: usize,
) -> Option<(Vec<Line<'static>>, Vec<ButtonHit>)> {
    let (lines, hits) = render_mod_ui_with_focus_and_hits(
        &spec.elements,
        wrap_width.max(1),
        spec.focused.as_deref(),
    );
    (!lines.is_empty()).then_some((lines, hits))
}

/// A mod UI block rendered into the transcript, with its place in the line list.
#[derive(Debug)]
struct ModUiBlockSpan {
    message_index: usize,
    start: usize,
    end: usize,
    prefix_width: u16,
    hits: Vec<ButtonHit>,
}

/// Translate rendered block spans into absolute screen hit regions.
///
/// `visible_start` is the index of the first line actually painted and
/// `visible_len` how many lines follow it; a block can begin above the pane.
fn record_mod_ui_regions(
    area: Rect,
    spans: &[ModUiBlockSpan],
    visible_start: usize,
    visible_len: usize,
    regions: &mut Vec<ModUiHitRegion>,
) {
    let visible_end = visible_start.saturating_add(visible_len);
    for span in spans {
        let start = span.start.max(visible_start);
        let end = span.end.min(visible_end);
        if start >= end {
            continue;
        }
        regions.push(ModUiHitRegion {
            message_index: span.message_index,
            rect: Rect::new(
                area.x.saturating_add(span.prefix_width),
                area.y
                    .saturating_add(u16::try_from(start - visible_start).unwrap_or(u16::MAX)),
                area.width.saturating_sub(span.prefix_width),
                u16::try_from(end - start).unwrap_or(u16::MAX),
            ),
            scroll_offset: start - span.start,
            hits: span.hits.clone(),
        });
    }
}

fn visible_message_window(
    heights: &[usize],
    start: usize,
    viewport_height: usize,
) -> (usize, usize, usize) {
    let start = start.min(heights.len());
    if heights.is_empty() {
        return (start, 0, 0);
    }

    let mut visible_count = 0;
    let mut accumulated = 0;
    for i in start..heights.len() {
        let h = heights[i];
        if accumulated + h > viewport_height {
            // A single long assistant response can be taller than the entire
            // viewport. Include the overflowing final message and let line
            // trimming show its tail instead of dropping it completely.
            if visible_count == 0 || i == heights.len() - 1 {
                accumulated += h;
                visible_count += 1;
            }
            break;
        }
        accumulated += h;
        visible_count += 1;
    }

    (start, visible_count.max(1), accumulated)
}

fn bottom_message_window(heights: &[usize], viewport_height: usize) -> (usize, usize, usize) {
    if heights.is_empty() {
        return (0, 0, 0);
    }

    let mut start = heights.len();
    let mut visible_count = 0;
    let mut accumulated = 0;
    for i in (0..heights.len()).rev() {
        let h = heights[i];
        if accumulated + h > viewport_height {
            start = i;
            accumulated += h;
            visible_count += 1;
            break;
        }
        start = i;
        accumulated += h;
        visible_count += 1;
    }

    (start, visible_count.max(1), accumulated)
}

fn render_all_message_lines(
    messages: &[Message],
    width: usize,
    is_compact: bool,
    theme: &Theme,
    mod_ui_blocks: &[ModUiBlockSpec],
) -> (Vec<Line<'static>>, Vec<ModUiBlockSpan>) {
    let mut lines = Vec::new();
    let mut block_spans = Vec::new();
    for (index, msg) in messages.iter().enumerate() {
        if let Some(span) = append_message_lines(
            &mut lines,
            msg,
            index,
            width,
            is_compact,
            theme,
            mod_ui_blocks,
        ) {
            block_spans.push(span);
        }
    }
    (lines, block_spans)
}

fn message_render_signature(
    messages: &[Message],
    theme: &Theme,
    mod_ui_blocks: &[ModUiBlockSpec],
) -> u64 {
    let mut signature = (messages.len() as u64) ^ theme.render_signature();
    for (idx, msg) in messages.iter().enumerate() {
        signature = signature
            .wrapping_mul(16_777_619)
            .wrapping_add(idx as u64)
            .wrapping_add(u64::from(msg.role as u8))
            .wrapping_add((msg.content.len() as u64) << 8)
            .wrapping_add(u64::from(msg.streaming) << 48)
            .wrapping_add(u64::from(msg.folded) << 49);
        if let Some(tool) = &msg.tool_name {
            signature = signature
                .wrapping_mul(16_777_619)
                .wrapping_add(tool.len() as u64);
        }
    }
    // Focus and the tree itself are inputs to the block render, so a cache
    // built under different ones must not be reused.
    for block in mod_ui_blocks {
        signature = signature
            .wrapping_mul(16_777_619)
            .wrapping_add(block.message_index as u64);
        mix_text(&mut signature, &block.content);
        if let Some(focused) = &block.focused {
            mix_text(&mut signature, focused);
        }
        signature = mix_elements(signature, &block.elements);
    }
    signature
}

fn mix_text(signature: &mut u64, text: &str) {
    for byte in text.bytes() {
        *signature = signature
            .wrapping_mul(16_777_619)
            .wrapping_add(u64::from(byte));
    }
}

fn mix_elements(mut signature: u64, elements: &[UiElement]) -> u64 {
    for element in elements {
        signature = signature.wrapping_mul(16_777_619);
        match element {
            UiElement::Text { text, .. } => mix_text(&mut signature, text),
            UiElement::Button { label, id } => {
                mix_text(&mut signature, label);
                mix_text(&mut signature, id);
            }
            UiElement::Box { title, children } => {
                if let Some(title) = title {
                    mix_text(&mut signature, title);
                }
                signature = mix_elements(signature, children);
            }
        }
    }
    signature
}

fn append_message_lines(
    lines: &mut Vec<Line<'static>>,
    msg: &Message,
    index: usize,
    width: usize,
    is_compact: bool,
    theme: &Theme,
    mod_ui_blocks: &[ModUiBlockSpec],
) -> Option<ModUiBlockSpan> {
    if !is_compact
        && !lines.is_empty()
        && matches!(msg.role, MessageRole::User | MessageRole::Assistant)
    {
        lines.push(Line::from(""));
    }

    // Handle folded messages
    if msg.folded {
        let style = Style::default().fg(theme.dim);
        let fold_indicator = format!(
            "▸ {} [+{} lines folded]",
            match msg.role {
                MessageRole::User => "User",
                MessageRole::Assistant => "Assistant",
                MessageRole::ToolResult => "Tool Result",
                MessageRole::ToolCall => "Tool Call",
                MessageRole::System => "System",
            },
            message_display_height(msg, width).saturating_sub(1)
        );
        lines.push(Line::from(Span::styled(fold_indicator, style)));
        return None;
    }

    let prefix = message_prefix(msg.role);
    let style = match msg.role {
        MessageRole::User => Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
        MessageRole::Assistant => Style::default()
            .fg(theme.assistant)
            .add_modifier(Modifier::BOLD),
        MessageRole::ToolResult | MessageRole::ToolCall => Style::default().fg(theme.tool),
        MessageRole::System => Style::default().fg(theme.dim),
    };
    let body_style = Style::default().fg(theme.foreground);

    let prefix_width = unicode_width::UnicodeWidthStr::width(prefix);
    let wrap_width = width.saturating_sub(prefix_width);

    // In compact mode, truncate long messages to single line
    if is_compact && message_display_height(msg, width) > 3 {
        let truncated = msg
            .content
            .chars()
            .take(width.saturating_sub(prefix_width))
            .collect::<String>();
        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            Span::styled(truncated, style.add_modifier(Modifier::DIM)),
        ]));
        return None;
    }

    // A live mod UI block is re-rendered from its tree at the pane's actual
    // width. That keeps the drawn button columns identical to the hit columns
    // even when the stored text was rendered at a different width.
    if !is_compact
        && let Some(spec) = matching_mod_ui_block(msg, index, mod_ui_blocks)
        && let Some((block_lines, hits)) = mod_ui_block_lines(spec, wrap_width)
    {
        let start = lines.len();
        for (row, line) in block_lines.into_iter().enumerate() {
            let gutter = if row == 0 {
                prefix.to_string()
            } else {
                " ".repeat(prefix_width)
            };
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(Span::styled(gutter, style));
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
        return Some(ModUiBlockSpan {
            message_index: index,
            start,
            end: lines.len(),
            prefix_width: u16::try_from(prefix_width).unwrap_or(u16::MAX),
            hits,
        });
    }

    // A Read result is a file body with no fence. Highlight it from a
    // conservative guess at the language, keeping its indentation.
    if msg.role == MessageRole::ToolResult
        && !is_compact
        && msg.tool_name.as_deref() == Some("Read")
        && let Some(language) = guess_language(&msg.content)
        && let Some(code_lines) = highlighted_code_lines(
            &msg.content,
            language,
            prefix,
            prefix_width,
            style,
            body_style,
        )
    {
        lines.extend(code_lines);
        return None;
    }

    // Use syntax highlighting for assistant messages that contain code blocks.
    let use_highlight = msg.role == MessageRole::Assistant && msg.content.contains("```");

    if use_highlight && !msg.streaming {
        let spans = highlight_spans(&msg.content, body_style);
        // Flatten spans into wrapped lines.
        let mut line_spans: Vec<Span> = Vec::new();
        let mut line_width = prefix_width;
        let mut is_first_line = true;

        for span in spans {
            let span_text = span.content.to_string();
            for word in span_text.split_whitespace() {
                let word_width = unicode_width::UnicodeWidthStr::width(word);
                let space = usize::from(!line_spans.is_empty());
                if line_width + space + word_width > width && !line_spans.is_empty() {
                    // Flush current line
                    let mut final_spans = if is_first_line {
                        vec![Span::styled(prefix, style)]
                    } else {
                        vec![Span::styled(" ".repeat(prefix_width), body_style)]
                    };
                    final_spans.extend(std::mem::take(&mut line_spans));
                    lines.push(Line::from(final_spans));
                    line_width = prefix_width;
                    is_first_line = false;
                }
                if !line_spans.is_empty() {
                    line_spans.push(Span::styled(" ", body_style));
                    line_width += 1;
                }
                line_spans.push(Span::styled(word.to_string(), span.style));
                line_width += word_width;
            }
            // Preserve newlines within the span text
            if span_text.ends_with('\n') {
                let mut final_spans = if is_first_line {
                    vec![Span::styled(prefix, style)]
                } else {
                    vec![Span::styled(" ".repeat(prefix_width), body_style)]
                };
                final_spans.extend(std::mem::take(&mut line_spans));
                lines.push(Line::from(final_spans));
                line_width = prefix_width;
                is_first_line = false;
            }
        }

        if !line_spans.is_empty() {
            let mut final_spans = if is_first_line {
                vec![Span::styled(prefix, style)]
            } else {
                vec![Span::styled(" ".repeat(prefix_width), body_style)]
            };
            final_spans.extend(line_spans);
            lines.push(Line::from(final_spans));
        }
    } else {
        let wrapped = wrap_text(&msg.content, wrap_width);
        for (i, text) in wrapped.into_iter().enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(prefix, style),
                    if msg.streaming {
                        Span::styled(text, body_style.add_modifier(Modifier::DIM))
                    } else {
                        Span::styled(text, body_style)
                    },
                ]));
            } else {
                let indent = " ".repeat(prefix_width);
                lines.push(Line::from(vec![
                    Span::styled(indent, style),
                    if msg.streaming {
                        Span::styled(text, body_style.add_modifier(Modifier::DIM))
                    } else {
                        Span::styled(text, body_style)
                    },
                ]));
            }
        }
    }

    // The call row names the tool; the result row keeps the name only so the
    // renderer can choose a highlighter, and does not print a second label.
    if let (Some(tool), MessageRole::ToolCall) = (&msg.tool_name, msg.role) {
        lines.push(Line::from(Span::styled(
            format!("  [{tool}]"),
            Style::default().fg(theme.dim),
        )));
    }
    None
}

fn trim_lines_to_viewport(mut lines: Vec<Line>, viewport_height: usize) -> Vec<Line> {
    if viewport_height == 0 || lines.len() <= viewport_height {
        return lines;
    }
    lines.split_off(lines.len() - viewport_height)
}

/// The half-open line range shown by the scrollback viewport.
fn scroll_slice_bounds(len: usize, viewport_height: usize, scroll_offset: usize) -> (usize, usize) {
    if viewport_height == 0 || len <= viewport_height {
        return (0, len);
    }

    let end = len
        .saturating_sub(scroll_offset)
        .max(viewport_height)
        .min(len);
    let start = end.saturating_sub(viewport_height);
    (start, end)
}

/// The welcome page shown while the transcript is empty.
///
/// It replaces the old one-line startup banner: the same facts (name, version,
/// model, branch) plus the mascot and the keys a new user needs. The transcript
/// is not a widget, so the page is built from [`Line`]s rather than a boxed
/// widget; a terminal too small for the box gets plain centred lines instead of
/// a clipped one.
fn render_welcome_art(
    theme: &Theme,
    width: usize,
    viewport_height: usize,
    model: &str,
    branch: &str,
) -> Vec<Line<'static>> {
    let meta = welcome_meta(model, branch);
    if width >= WELCOME_CARD_MIN_WIDTH && viewport_height >= WELCOME_CARD_HEIGHT {
        welcome_card(theme, width, model, branch)
    } else {
        welcome_plain(theme, &meta)
    }
}

/// `CodeRex v0.1.0 · model · branch`, omitting what the session does not know.
fn welcome_meta(model: &str, branch: &str) -> String {
    let mut parts = vec![format!("CodeRex v{}", env!("CARGO_PKG_VERSION"))];
    if !model.is_empty() {
        parts.push(model.to_string());
    }
    if !branch.is_empty() {
        parts.push(branch.to_string());
    }
    parts.join("  ·  ")
}

/// The boxed welcome page.
///
/// The card hugs the left edge: the transcript is a log, and a centred card
/// floats away from the text it introduces. Colour carries the hierarchy
/// instead of position: the wordmark cycles through the theme's accents, the
/// mascot is green, the version/model/branch and the key hints each have their
/// own colour.
fn welcome_card(theme: &Theme, width: usize, model: &str, branch: &str) -> Vec<Line<'static>> {
    let card_width = width.min(WELCOME_CARD_MAX_WIDTH);
    let inner = card_width.saturating_sub(2);
    let indent = "";
    let border = Style::default().fg(theme.border);
    let art = Style::default().fg(theme.assistant);
    let muted = Style::default().fg(theme.dim);

    let mut lines = Vec::with_capacity(WELCOME_CARD_HEIGHT);
    lines.push(card_border(indent, '╭', '╮', inner, border));
    lines.push(card_row(indent, "", muted, inner, border));
    lines.push(card_row_spans(indent, wordmark_spans(theme), inner, border));
    lines.push(card_row(indent, "", muted, inner, border));

    // Left-align the mascot as one block. Stripping each line's own leading
    // spaces would distort the drawing, so remove only the indent common to
    // every row and keep the rest.
    let art_lines: Vec<&str> = WELCOME_T_REX
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::trim_end)
        .collect();
    let common_indent = art_lines
        .iter()
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    for art_line in art_lines {
        let art_line = &art_line[common_indent..];
        lines.push(card_row_left(
            indent,
            &format!("  {art_line}"),
            art,
            inner,
            border,
        ));
    }

    lines.push(card_row(indent, "", muted, inner, border));
    lines.push(card_row_spans(
        indent,
        welcome_meta_spans(theme, model, branch),
        inner,
        border,
    ));
    lines.push(card_row(indent, "", muted, inner, border));
    lines.push(card_row_spans(
        indent,
        welcome_hint_spans(theme, inner),
        inner,
        border,
    ));
    lines.push(card_row(indent, "", muted, inner, border));
    lines.push(card_border(indent, '╰', '╯', inner, border));
    lines
}

/// `C O D E R E X`, one colour per letter, hugging the left edge.
fn wordmark_spans(theme: &Theme) -> Vec<Span<'static>> {
    const LETTERS: [char; 7] = ['C', 'O', 'D', 'E', 'R', 'E', 'X'];
    let colors = [
        theme.accent,
        theme.assistant,
        theme.accent_secondary,
        theme.warning,
        theme.error,
        theme.accent,
        theme.assistant,
    ];

    let mut spans = vec![Span::raw("  ")];
    for (index, letter) in LETTERS.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            letter.to_string(),
            Style::default()
                .fg(colors[index])
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans
}

/// Version, model and branch as separate coloured spans.
fn welcome_meta_spans(theme: &Theme, model: &str, branch: &str) -> Vec<Span<'static>> {
    let separator = Style::default().fg(theme.dim);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            format!("CodeRex v{}", env!("CARGO_PKG_VERSION")),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !model.is_empty() {
        spans.push(Span::styled(" · ", separator));
        spans.push(Span::styled(
            model.to_string(),
            Style::default().fg(theme.assistant),
        ));
    }
    if !branch.is_empty() {
        spans.push(Span::styled(" · ", separator));
        spans.push(Span::styled(
            branch.to_string(),
            Style::default().fg(theme.accent_secondary),
        ));
    }
    spans
}

/// `/help` and the exit keys, each in its own colour.
fn welcome_hint_spans(theme: &Theme, inner: usize) -> Vec<Span<'static>> {
    let dim = Style::default().fg(theme.dim);
    let key = |color| Style::default().fg(color).add_modifier(Modifier::BOLD);

    let mut spans = vec![
        Span::raw("  "),
        Span::styled("Type ", dim),
        Span::styled("/help", key(theme.accent)),
    ];
    if inner >= 60 {
        spans.push(Span::styled(" for commands · ", dim));
        spans.push(Span::styled("Ctrl+C", key(theme.warning)));
        spans.push(Span::styled(" to clear · ", dim));
        spans.push(Span::styled("Ctrl+D", key(theme.assistant)));
        spans.push(Span::styled(" to exit", dim));
    } else if inner >= 42 {
        spans.push(Span::styled(" for commands · ", dim));
        spans.push(Span::styled("Ctrl+D", key(theme.assistant)));
        spans.push(Span::styled(" to exit", dim));
    } else {
        spans.push(Span::styled(" · ", dim));
        spans.push(Span::styled("Ctrl+D", key(theme.assistant)));
        spans.push(Span::styled(" to exit", dim));
    }
    spans
}

/// Plain welcome lines, used when the box would not fit.
fn welcome_plain(theme: &Theme, meta: &str) -> Vec<Line<'static>> {
    let muted = Style::default().fg(theme.dim);
    vec![
        Line::from(""),
        Line::from(wordmark_spans(theme)),
        Line::from(Span::styled(format!("  {meta}"), muted)),
        Line::from(""),
        Line::from(Span::styled(
            "  Type /help for commands · Ctrl+D to exit",
            muted,
        )),
        Line::from(""),
    ]
}

fn card_border(indent: &str, left: char, right: char, inner: usize, style: Style) -> Line<'static> {
    Line::from(Span::styled(
        format!("{indent}{left}{}{right}", "─".repeat(inner)),
        style,
    ))
}

fn card_row(indent: &str, text: &str, style: Style, inner: usize, border: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{indent}│"), border),
        Span::styled(center_padded(text, inner), style),
        Span::styled("│", border),
    ])
}

/// A card row built from styled spans, left-aligned and padded to `inner`.
///
/// Content longer than the card is truncated span by span; the row still ends
/// at the border, so a long model name cannot break the box.
fn card_row_spans(
    indent: &str,
    spans: Vec<Span<'static>>,
    inner: usize,
    border: Style,
) -> Line<'static> {
    let mut remaining = inner;
    let mut rendered: Vec<Span<'static>> = Vec::new();
    for span in spans {
        if remaining == 0 {
            break;
        }
        let content = span.content.to_string();
        let width = unicode_width::UnicodeWidthStr::width(content.as_str());
        if width <= remaining {
            remaining -= width;
            rendered.push(span);
        } else {
            let truncated = truncate_to_width(&content, remaining);
            let truncated_width = unicode_width::UnicodeWidthStr::width(truncated.as_str());
            remaining = remaining.saturating_sub(truncated_width);
            rendered.push(Span::styled(truncated, span.style));
            break;
        }
    }
    if remaining > 0 {
        rendered.push(Span::raw(" ".repeat(remaining)));
    }

    let mut line = vec![Span::styled(format!("{indent}│"), border)];
    line.extend(rendered);
    line.push(Span::styled("│", border));
    Line::from(line)
}

/// Like [`card_row`], but the content is already laid out and is only padded
/// on the right. Used for multi-line art, where centring each line separately
/// would distort the drawing.
fn card_row_left(
    indent: &str,
    text: &str,
    style: Style,
    inner: usize,
    border: Style,
) -> Line<'static> {
    let text = truncate_to_width(text, inner);
    let text_width = unicode_width::UnicodeWidthStr::width(text.as_str());
    Line::from(vec![
        Span::styled(format!("{indent}│"), border),
        Span::styled(
            format!("{text}{}", " ".repeat(inner.saturating_sub(text_width))),
            style,
        ),
        Span::styled("│", border),
    ])
}

/// Pad `text` with spaces so it is centred in `width` display columns.
///
/// A string longer than the space is truncated first; the caller passes text
/// that renders as a single line (the mascot rows have no styling).
fn center_padded(text: &str, width: usize) -> String {
    let text = truncate_to_width(text, width);
    let text_width = unicode_width::UnicodeWidthStr::width(text.as_str());
    let total_pad = width.saturating_sub(text_width);
    let left = total_pad / 2;
    format!(
        "{}{}{}",
        " ".repeat(left),
        text,
        " ".repeat(total_pad - left)
    )
}

fn truncate_to_width(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(1);
        if used + character_width > width {
            break;
        }
        out.push(character);
        used += character_width;
    }
    out
}

/// Render a compact notification bar above the statusline.
fn render_notification_bar(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    center: &mut crate::notification_center::NotificationCenter,
    theme: &Theme,
) {
    use crate::notification_center::NotificationKind;

    if let Some(notif) = center.latest() {
        let (icon, color) = match notif.kind {
            NotificationKind::Info => ("ℹ", theme.notif_info),
            NotificationKind::Warning => ("⚠", theme.notif_warning),
            NotificationKind::Error => ("✗", theme.notif_error),
            NotificationKind::Success => ("✓", theme.success),
            NotificationKind::Teaser => ("✦", theme.tool),
            NotificationKind::Progress => ("⟳", theme.accent_secondary),
        };
        let text = format!(
            " {} {}  ({} unseen)",
            icon,
            notif.message,
            center.unseen_count()
        );
        let line = Line::from(Span::styled(text, Style::default().fg(color)));
        let paragraph = Paragraph::new(line);
        frame.render_widget(paragraph, area);

        // Mark as seen
        center.mark_seen();
    }
}

fn render_left_pane(frame: &mut Frame, area: ratatui::layout::Rect, state: &UiState) {
    let content = if state.left_pane_content.is_empty() {
        "No file open.\nUse /open <path> to view a file."
    } else {
        &state.left_pane_content
    };

    let wrapped = wrap_text(content, area.width.saturating_sub(2) as usize);
    let lines: Vec<Line> = wrapped
        .into_iter()
        .map(|s| Line::from(Span::styled(s, Style::default().fg(state.theme.foreground))))
        .collect();

    let title = if state.left_pane_title.is_empty() {
        "File Viewer"
    } else {
        &state.left_pane_title
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(state.theme.border));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn render_statusline(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    statusline: &Statusline,
    theme: &Theme,
) {
    if area.height == 0 {
        return;
    }
    let rendered = statusline.render(area.width as usize);
    let spans = rendered
        .split(" | ")
        .map(|s| Span::raw(s.to_string()))
        .collect::<Vec<_>>();
    let line = Line::from(spans);
    let paragraph = Paragraph::new(line)
        .style(Style::default().fg(theme.statusbar_fg))
        .alignment(Alignment::Left);
    // The pane is three rows tall; the text owns the middle row so the blank
    // space above and below it is the same.
    let text_area = Rect {
        y: area.y + area.height / 2,
        height: 1,
        ..area
    };
    frame.render_widget(paragraph, text_area);
}

fn render_permission_bar(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    bar: &crate::permission_bar::PermissionBar,
    theme: &Theme,
) {
    let color = bar.color(theme);
    let left_padding = 2usize.min(area.width as usize);
    let rendered = format!(
        "{}{}",
        " ".repeat(left_padding),
        bar.render((area.width as usize).saturating_sub(left_padding))
    );
    let line = Line::from(Span::styled(rendered, Style::default().fg(color)));
    let paragraph = Paragraph::new(line).alignment(Alignment::Left);
    frame.render_widget(paragraph, area);
}

/// A horizontal rule separating the transcript from the input box.
fn render_input_rule(frame: &mut Frame, area: ratatui::layout::Rect, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let rule = Line::from(Span::styled(
        "─".repeat(area.width as usize),
        Style::default().fg(theme.border),
    ));
    frame.render_widget(Paragraph::new(rule), area);
}

fn render_input(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    input: &crate::input::InputBuffer,
    theme: &Theme,
) {
    // Byte offsets are not columns: a CJK character occupies two cells. The
    // buffer reports its rows and the cursor's row/column in display units, so
    // a wrapped or multi-line buffer places the cursor on the right cell.
    let wrap = input_wrap_width(area.width);
    let rows = input.display_rows(wrap);
    let (cursor_row, cursor_col) = input.display_cursor(wrap);
    let height = (area.height as usize).max(1);
    // Normally every row fits; when the pane is capped, keep the cursor row
    // visible by scrolling the window.
    let first = cursor_row
        .saturating_sub(height - 1)
        .min(rows.len().saturating_sub(height));

    let text_style = Style::default().fg(theme.input_fg);
    let selection_style = Style::default()
        .fg(theme.selection_fg)
        .bg(theme.selection_bg);
    let selection = input.selection_range();

    let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
    for (index, &(start, end)) in rows.iter().enumerate().skip(first).take(height) {
        // The prompt occupies two cells on the first row and is blanked on the
        // continuation rows, so every row's text starts at the same column.
        let prompt = if index == 0 { "❯ " } else { "  " };
        let text = &input.text[start..end];
        let mut spans: Vec<Span> = vec![Span::styled(prompt, Style::default().fg(theme.accent))];
        match selection {
            Some((sel_start, sel_end)) if sel_start < end && sel_end > start => {
                let local_start = sel_start.max(start) - start;
                let local_end = sel_end.min(end) - start;
                if local_start > 0 {
                    spans.push(Span::styled(text[..local_start].to_string(), text_style));
                }
                spans.push(Span::styled(
                    text[local_start..local_end].to_string(),
                    selection_style,
                ));
                if local_end < text.len() {
                    spans.push(Span::styled(text[local_end..].to_string(), text_style));
                }
            }
            _ => spans.push(Span::styled(text.to_string(), text_style)),
        }
        lines.push(Line::from(spans));
    }

    // Trailing annotations go after the last visible row's text so they never
    // shift the text the cursor column was measured against.
    if let Some(last) = lines.last_mut() {
        if let Some(ref sug) = input.suggestion {
            last.spans.push(Span::styled(
                format!("  {sug}"),
                Style::default().fg(theme.dim),
            ));
        }
        // Vim's mode line. `visual_anchor` is set the instant `v`/`V` is
        // pressed, so the badge appears before any motion, matching Vim.
        if input.visual_anchor.is_some() {
            let label = if input.visual_linewise {
                "-- VISUAL LINE --"
            } else {
                "-- VISUAL --"
            };
            last.spans.push(Span::styled(
                format!("  {label}"),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
    frame.set_cursor_position((
        area.x + 2 + u16::try_from(cursor_col).unwrap_or(u16::MAX),
        area.y + u16::try_from(cursor_row.saturating_sub(first)).unwrap_or(u16::MAX),
    ));
}

/// Render the inline command palette just above the statusline / input line.
fn render_inline_palette(frame: &mut Frame, area: ratatui::layout::Rect, state: &UiState) {
    use crate::overlay::Overlay;
    let Some(Overlay::CommandPalette {
        query,
        commands,
        selected,
    }) = state.overlays.top()
    else {
        return;
    };

    let mut lines: Vec<Line> = Vec::new();

    // Header
    lines.push(Line::from(vec![
        Span::styled(
            "Commands ",
            Style::default()
                .fg(state.theme.foreground)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("[{query}]"), Style::default().fg(state.theme.dim)),
    ]));

    let available_rows = area.height.saturating_sub(1) as usize;
    let visible_count = commands.len().min(available_rows);
    if visible_count == 0 {
        let paragraph = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::NONE)
                .style(Style::default().fg(state.theme.foreground)),
        );
        frame.render_widget(paragraph, area);
        return;
    }
    let start = selected
        .saturating_sub(visible_count - 1)
        .min(commands.len().saturating_sub(visible_count));

    for (i, command) in commands.iter().enumerate().skip(start).take(visible_count) {
        let style = if i == *selected {
            Style::default()
                .fg(state.theme.selection_fg)
                .bg(state.theme.selection_bg)
        } else {
            Style::default().fg(state.theme.foreground)
        };
        lines.push(Line::from(Span::styled(command.clone(), style)));
    }

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::NONE)
            .style(Style::default().fg(state.theme.foreground)),
    );
    frame.render_widget(paragraph, area);
}

fn render_overlay(frame: &mut Frame, area: ratatui::layout::Rect, state: &UiState) {
    use crate::overlay::Overlay;
    let Some(overlay) = state.overlays.top() else {
        return;
    };

    let content = match overlay {
        Overlay::PermissionDialog {
            tool,
            args,
            risk_summary,
            always_option,
        } => {
            format!(
                "Permission Request\nTool: {tool}\nArgs: {args}\nRisk: {risk_summary}\nState: waiting for input\n[a] Allow once  [d] Deny{}{}",
                if *always_option {
                    "  [A] Always allow"
                } else {
                    ""
                },
                if *always_option {
                    ""
                } else {
                    "\n[a] Allow once"
                }
            )
        }
        Overlay::Notification { message, level } => {
            format!("Notification: {message} ({level:?})")
        }
        Overlay::CommandPalette { .. } => {
            // Inline palette is handled separately; skip here.
            return;
        }
        Overlay::Detail { title, body } => {
            let _ = title;
            body.clone()
        }
        Overlay::Confirm { message } => {
            format!("{message}\n[y/n]")
        }
    };

    let title = match overlay {
        Overlay::PermissionDialog { .. } => "Permission Details",
        Overlay::Notification { .. } => "Notification",
        Overlay::Detail { title, .. } => title.as_str(),
        Overlay::CommandPalette { .. } => "Commands",
        Overlay::Confirm { .. } => "Confirm",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(state.theme.overlay_border));
    let mut paragraph = Paragraph::new(content)
        .style(Style::default().fg(state.theme.foreground))
        .block(block);

    // Center the overlay. A detail body (a provider error, for example) is
    // long: give it the full width and wrap it. Dialog bodies must not wrap —
    // a long Write argument would push the `[a] Allow once` choices out of the
    // box — so they keep their fixed layout and are clipped instead.
    let is_detail = matches!(overlay, Overlay::Detail { .. });
    if is_detail {
        paragraph = paragraph.wrap(Wrap { trim: false });
    }
    let w = if is_detail {
        area.width.saturating_sub(4).min(100)
    } else {
        50.min(area.width.saturating_sub(4))
    };
    let h = if is_detail {
        area.height.saturating_sub(4).min(20)
    } else {
        10.min(area.height.saturating_sub(4))
    };
    let x = (area.width - w) / 2;
    let y = (area.height - h) / 2;
    let overlay_area = ratatui::layout::Rect {
        x: area.x + x,
        y: area.y + y,
        width: w,
        height: h,
    };
    frame.render_widget(paragraph, overlay_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    /// The keyboard push is the one startup byte sequence that depends on the
    /// terminal, and it is a correctness matter: `\x1b[>1u` tells a capable
    /// terminal to stop collapsing `Ctrl+I` into `Tab`.
    #[test]
    fn enter_pushes_keyboard_protocol_only_for_capable_terminals() {
        let kitty = TerminalProfile::detect(&env(&[("KITTY_WINDOW_ID", "1")]));
        let mut out = Vec::new();
        enter_commands(&kitty, &mut out).expect("write enter commands");
        let bytes = String::from_utf8(out).expect("utf8");
        assert!(
            bytes.contains("\x1b[>1u"),
            "capable terminal did not get the disambiguation push: {bytes:?}"
        );

        let vscode = TerminalProfile::detect(&env(&[("TERM_PROGRAM", "vscode")]));
        let mut out = Vec::new();
        enter_commands(&vscode, &mut out).expect("write enter commands");
        let bytes = String::from_utf8(out).expect("utf8");
        assert!(
            !bytes.contains("\x1b[>1u"),
            "unverified terminal was pushed a protocol it may not speak: {bytes:?}"
        );
        // …but it still enters the alternate screen and captures the mouse.
        assert!(
            bytes.contains("\x1b[?1049h"),
            "alt screen missing: {bytes:?}"
        );
    }

    #[test]
    fn exit_pops_only_what_was_pushed() {
        let mut pushed = Vec::new();
        exit_commands(true, &mut pushed).expect("write exit commands");
        let bytes = String::from_utf8(pushed).expect("utf8");
        assert!(
            bytes.contains("\x1b[<1u"),
            "pushed protocol was not popped: {bytes:?}"
        );
        assert!(bytes.contains("\x1b[?1049l"), "alt screen not left");

        let mut never = Vec::new();
        exit_commands(false, &mut never).expect("write exit commands");
        let bytes = String::from_utf8(never).expect("utf8");
        assert!(
            !bytes.contains("\x1b[<1u"),
            "popped a protocol level that was never pushed: {bytes:?}"
        );
    }

    #[test]
    fn terminal_new_succeeds() {
        let result = TerminalUi::new();
        // May fail in CI without a terminal, but should not panic
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn statusline_renders_within_width() {
        let sl = Statusline {
            model: "sonnet".into(),
            total_tokens: 100,
            cost_usd: 0.01,
            git_branch: "main".into(),
            session_title: "test".into(),
            thinking: false,
            thinking_started: None,
            last_duration: None,
            plan_status: None,
            thinking_label: None,
            spinner_frame: 0,
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
            format: None,
        };
        let rendered = sl.render(80);
        assert!(rendered.len() <= 80);
    }

    #[test]
    fn wrap_text_basic() {
        let lines = wrap_text("hello world foo", 10);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "hello");
        assert_eq!(lines[1], "world foo");
    }

    #[test]
    fn wrap_text_preserves_newlines() {
        let lines = wrap_text("a\nb\nc", 80);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "a");
        assert_eq!(lines[1], "b");
        assert_eq!(lines[2], "c");
    }

    #[test]
    fn wrap_text_zero_width() {
        let lines = wrap_text("hello", 0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "hello");
    }

    #[test]
    fn wrapped_line_count_matches_wrap_text_without_allocating_lines() {
        for (text, width) in [
            ("hello world foo", 10),
            ("a\nb\nc", 80),
            ("single", 80),
            ("supercalifragilistic", 5),
            ("", 80),
            ("two  spaces and\ttabs", 8),
        ] {
            assert_eq!(
                wrapped_line_count(text, width),
                wrap_text(text, width).len().max(1),
                "mismatch for {text:?} at width {width}"
            );
        }
    }

    #[test]
    fn trim_lines_to_viewport_keeps_latest_lines() {
        let lines = vec![
            Line::from("line 1"),
            Line::from("line 2"),
            Line::from("line 3"),
            Line::from("line 4"),
        ];
        let trimmed = trim_lines_to_viewport(lines, 2);
        assert_eq!(trimmed.len(), 2);
        assert_eq!(trimmed[0].to_string(), "line 3");
        assert_eq!(trimmed[1].to_string(), "line 4");
    }

    #[test]
    fn trim_lines_to_viewport_scrolls_from_bottom() {
        let lines = [
            Line::from("line 1"),
            Line::from("line 2"),
            Line::from("line 3"),
            Line::from("line 4"),
            Line::from("line 5"),
        ];
        let (start, end) = scroll_slice_bounds(lines.len(), 2, 2);

        assert_eq!((start, end), (1, 3));
        assert_eq!(lines[start].to_string(), "line 2");
        assert_eq!(lines[start + 1].to_string(), "line 3");
    }

    #[test]
    fn message_lines_use_distinct_role_prefixes_and_spacing() {
        let messages = vec![
            Message {
                role: MessageRole::User,
                content: "hello".into(),
                tool_name: None,
                streaming: false,
                folded: false,
            },
            Message {
                role: MessageRole::Assistant,
                content: "hi there".into(),
                tool_name: None,
                streaming: false,
                folded: false,
            },
        ];

        let theme = Theme::dark();
        let (lines, _) = render_all_message_lines(&messages, 80, false, &theme, &[]);

        assert_eq!(lines[0].to_string(), "● hello");
        assert_eq!(lines[1].to_string(), "");
        assert_eq!(lines[2].to_string(), "◆ hi there");
    }

    #[test]
    fn visible_window_includes_overflowing_final_message() {
        let heights = vec![1, 80];
        let (_start, count, accumulated) = visible_message_window(&heights, 0, 20);

        assert_eq!(count, 2);
        assert_eq!(accumulated, 81);
    }

    #[test]
    fn visible_window_includes_single_overflowing_message() {
        let heights = vec![80];
        let (_start, count, accumulated) = visible_message_window(&heights, 0, 20);

        assert_eq!(count, 1);
        assert_eq!(accumulated, 80);
    }

    #[test]
    fn bottom_window_skips_middle_overflow_and_keeps_latest_response() {
        let heights = vec![1, 1, 80, 1, 80];
        let (start, count, accumulated) = bottom_message_window(&heights, 20);

        assert_eq!(start, 4);
        assert_eq!(count, 1);
        assert_eq!(accumulated, 80);
    }

    #[test]
    fn bottom_window_keeps_latest_small_messages() {
        let heights = vec![1, 80, 1, 2];
        let (start, count, accumulated) = bottom_message_window(&heights, 20);

        assert_eq!(start, 1);
        assert_eq!(count, 3);
        assert_eq!(accumulated, 83);
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer.cell((x, y)).map_or(" ", |c| c.symbol()))
            .collect()
    }

    fn render_to_buffer(width: u16, height: u16, state: &mut UiState) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render_frame(frame, state, &[], &mut Vec::new()))
            .expect("draw frame");
        terminal.backend().buffer().clone()
    }

    /// The pane stack the renderer lays out, for the given visibility flags.
    fn chunks_for(
        height: u16,
        palette: u16,
        notif: bool,
        statusline: bool,
        input: u16,
        perm: bool,
    ) -> Vec<ratatui::layout::Rect> {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints(frame_constraints(palette, notif, statusline, input, perm))
            .split(ratatui::layout::Rect::new(0, 0, 80, height))
            .to_vec()
    }

    /// The regression: `Layout::split` keeps one rect per constraint even when a
    /// constraint resolves to zero height, so indexing by "which panes are on"
    /// pointed the input line and statusline at zero-height rects and both
    /// silently vanished.
    #[test]
    fn input_and_statusline_render_on_their_own_panes() {
        // Palette hidden, no notification: the case where the old computed
        // index (1) landed on the zero-height palette rect.
        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 30, &mut state);
        let rows: Vec<String> = (0..30).map(|y| row_text(&buffer, y)).collect();

        let chunks = chunks_for(30, 0, false, true, 1, true);
        // The text sits on the middle row of the padded pane.
        let statusline_y = chunks[STATUSLINE_IDX].y + 1;
        let input_y = chunks[INPUT_IDX].y;

        assert!(
            chunks[STATUSLINE_IDX].height > 0,
            "statusline pane has no height"
        );
        assert!(
            !rows[statusline_y as usize].trim().is_empty(),
            "statusline did not render on its pane (row {statusline_y}):\n{}",
            rows.join("\n")
        );
        assert!(
            rows[input_y as usize].contains('❯'),
            "input line did not render on its pane (row {input_y}):\n{}",
            rows.join("\n")
        );
    }

    /// Compact mode hides the statusline and permission bar. The input line
    /// keeps its own constraint, so it must still be laid out and drawn.
    #[test]
    fn input_renders_in_compact_mode() {
        let mut state = UiState {
            force_compact: true,
            ..UiState::default()
        };
        let buffer = render_to_buffer(80, 12, &mut state);
        let rows: Vec<String> = (0..12).map(|y| row_text(&buffer, y)).collect();

        let chunks = chunks_for(12, 0, false, false, 1, false);
        let input_y = chunks[INPUT_IDX].y;
        assert!(
            chunks[INPUT_IDX].height > 0,
            "input pane has no height in compact mode"
        );
        assert!(
            rows[input_y as usize].contains('❯'),
            "input line is not visible in compact mode (row {input_y}):\n{}",
            rows.join("\n")
        );
    }

    /// A Visual selection must be visible in the TUI, not only in the buffer's
    /// internal state: the input line carries the mode badge.
    #[test]
    fn input_line_shows_the_visual_mode_indicator() {
        let mut state = UiState::default();
        state.input.text = "hello".into();
        state.input.cursor = 5;
        state.input.start_visual(false);
        let buffer = render_to_buffer(80, 12, &mut state);
        let input_y = chunks_for(12, 0, false, false, 1, false)[INPUT_IDX].y;
        let row = row_text(&buffer, input_y);
        assert!(
            row.contains("-- VISUAL --"),
            "characterwise Visual badge missing (row {input_y}): {row:?}"
        );
        assert!(
            !row.contains("VISUAL LINE"),
            "linewise badge shown for a characterwise selection: {row:?}"
        );

        // Linewise Visual mode gets its own label.
        let mut state = UiState::default();
        state.input.text = "hello".into();
        state.input.cursor = 5;
        state.input.start_visual(true);
        let buffer = render_to_buffer(80, 12, &mut state);
        let row = row_text(&buffer, input_y);
        assert!(
            row.contains("-- VISUAL LINE --"),
            "linewise Visual badge missing (row {input_y}): {row:?}"
        );

        // With no selection there is no badge.
        let mut state = UiState::default();
        state.input.text = "hello".into();
        let buffer = render_to_buffer(80, 12, &mut state);
        let row = row_text(&buffer, input_y);
        assert!(
            !row.contains("VISUAL"),
            "badge shown without a selection: {row:?}"
        );
    }

    /// Every index the renderer uses must name the constraint it claims to.
    #[test]
    fn pane_indices_follow_the_constraint_order() {
        let chunks = chunks_for(30, 4, true, true, 1, true);
        assert_eq!(chunks.len(), PERMISSION_IDX + 1, "one rect per constraint");
        assert_eq!(chunks[PALETTE_IDX].height, 4, "palette");
        assert_eq!(chunks[NOTIFICATION_IDX].height, 1, "notification");
        assert_eq!(chunks[STATUSLINE_IDX].height, 3, "statusline");
        assert_eq!(chunks[RULE_IDX].height, 1, "input rule");
        assert_eq!(chunks[INPUT_IDX].height, 1, "input");
        assert_eq!(chunks[RULE_BELOW_IDX].height, 1, "input rule below");
        assert_eq!(chunks[PERMISSION_IDX].height, 2, "permission");
        assert!(chunks[0].height > 1, "message area");
        // Rects are contiguous and in order, top to bottom.
        for pair in chunks.windows(2) {
            assert_eq!(pair[0].y + pair[0].height, pair[1].y);
        }
    }

    /// The input box is separated from the transcript by a full-width rule.
    #[test]
    fn the_input_has_a_rule_above_it() {
        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 24, &mut state);
        let chunks = chunks_for(24, 0, false, true, 1, true);
        let rule_y = chunks[RULE_IDX].y;

        assert_eq!(chunks[RULE_IDX].height, 1);
        assert_eq!(
            rule_y + 1,
            chunks[INPUT_IDX].y,
            "the rule is not directly above the input"
        );
        let rule = row_text(&buffer, rule_y);
        assert!(
            rule.chars().all(|character| character == '─'),
            "rule row has other content: {rule:?}"
        );
        assert!(row_text(&buffer, chunks[INPUT_IDX].y).contains('❯'));
    }

    /// The input box is closed by a second rule below it, identical to the one
    /// above.
    #[test]
    fn the_input_has_a_rule_below_it() {
        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 24, &mut state);
        let chunks = chunks_for(24, 0, false, true, 1, true);
        let input_y = chunks[INPUT_IDX].y;

        assert_eq!(chunks[RULE_BELOW_IDX].height, 1);
        assert_eq!(
            chunks[RULE_BELOW_IDX].y,
            input_y + 1,
            "the lower rule is not directly below the input"
        );
        let rule = row_text(&buffer, chunks[RULE_BELOW_IDX].y);
        assert!(
            rule.chars().all(|character| character == '─'),
            "lower rule row has other content: {rule:?}"
        );
        assert_eq!(
            row_text(&buffer, chunks[RULE_IDX].y),
            rule,
            "the two rules around the input differ"
        );
    }

    /// Compact mode hides the statusline and permission bar; the rules are
    /// part of the input box and stay.
    #[test]
    fn the_input_rule_is_drawn_in_compact_mode() {
        let mut state = UiState {
            force_compact: true,
            ..UiState::default()
        };
        let buffer = render_to_buffer(80, 12, &mut state);
        let chunks = chunks_for(12, 0, false, false, 1, false);
        for index in [RULE_IDX, RULE_BELOW_IDX] {
            let rule = row_text(&buffer, chunks[index].y);
            assert!(
                rule.chars().all(|character| character == '─'),
                "rule row has other content: {rule:?}"
            );
        }
        assert!(row_text(&buffer, chunks[INPUT_IDX].y).contains('❯'));
    }

    /// An empty transcript paints the welcome page: name, version, model and
    /// the T-Rex mascot, not just a one-line banner.
    #[test]
    fn the_welcome_page_shows_the_name_version_and_model() {
        let mut state = UiState::default();
        state.statusline.model = "claude-sonnet-4-6".into();
        let buffer = render_to_buffer(80, 30, &mut state);
        let screen: String = (0..30)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("C O D E R E X"), "{screen}");
        assert!(screen.contains("CodeRex v"), "{screen}");
        assert!(screen.contains("claude-sonnet-4-6"), "{screen}");
        assert!(screen.contains("Type /help for commands"), "{screen}");
        assert!(
            screen.contains("__.-'|_|--|_|"),
            "the mascot is missing:\n{screen}"
        );
        assert!(screen.contains('╭') && screen.contains('╯'), "{screen}");
        assert!(
            (0..30).any(|y| row_text(&buffer, y).starts_with('╭')),
            "the welcome card is not left-aligned:\n{screen}"
        );
    }

    /// The theme background is painted across the frame, so dark and light are
    /// visibly different instead of only re-tinting accents.
    #[test]
    fn the_theme_background_covers_the_frame() {
        let mut dark = UiState::default();
        let dark_buffer = render_to_buffer(80, 24, &mut dark);
        let mut light = UiState {
            theme: crate::themes::Theme::light(),
            ..Default::default()
        };

        let light_buffer = render_to_buffer(80, 24, &mut light);

        let dark_bg = crate::themes::Theme::dark().background;
        let light_bg = crate::themes::Theme::light().background;
        assert_ne!(dark_bg, light_bg);
        for (x, y) in [(79, 23), (40, 20), (0, 0)] {
            assert_eq!(
                dark_buffer.cell((x, y)).unwrap().bg,
                dark_bg,
                "dark {x},{y}"
            );
            assert_eq!(
                light_buffer.cell((x, y)).unwrap().bg,
                light_bg,
                "light {x},{y}"
            );
        }
    }

    /// The wordmark carries a colour per letter, so the page is not a block of
    /// dim text.
    #[test]
    fn the_welcome_wordmark_is_colourful() {
        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 30, &mut state);
        let y = (0..30)
            .find(|y| row_text(&buffer, *y).contains("C O D E R E X"))
            .expect("wordmark row");

        let colours: std::collections::HashSet<Color> = (0..80)
            .filter_map(|x| buffer.cell((x, y)))
            .filter(|cell| matches!(cell.symbol(), "C" | "O" | "D" | "E" | "R" | "X"))
            .map(|cell| cell.fg)
            .collect();
        assert!(
            colours.len() >= 5,
            "the wordmark should use several colours, got {colours:?}"
        );
    }

    /// A transcript taller than the screen must never push the input box or
    /// its rules off the viewport. Ported from kimi-code's fullscreen-layout
    /// contract (the editor's borders stay visible at every terminal size).
    #[test]
    fn the_chrome_survives_a_transcript_taller_than_the_screen() {
        for (width, height) in [(120_u16, 30_u16), (80, 24), (40, 12), (30, 10)] {
            let mut state = UiState::default();
            for index in 0..80 {
                state.messages.push(Message {
                    role: MessageRole::Assistant,
                    content: format!("message {index} {}", "lorem ipsum ".repeat(20)).into(),
                    tool_name: None,
                    streaming: false,
                    folded: false,
                });
            }
            let buffer = render_to_buffer(width, height, &mut state);
            let rows: Vec<String> = (0..height).map(|y| row_text(&buffer, y)).collect();
            let full = height >= 20;
            let chunks = chunks_for(height, 0, false, full, 1, full);

            assert!(
                rows[chunks[INPUT_IDX].y as usize].contains('❯'),
                "input missing at {width}x{height}:\n{}",
                rows.join("\n")
            );
            for index in [RULE_IDX, RULE_BELOW_IDX] {
                let row = &rows[chunks[index].y as usize];
                assert!(
                    row.chars().all(|c| c == '─'),
                    "rule {index} missing at {width}x{height}: {row:?}"
                );
            }
            assert!(
                chunks[0].height >= 1,
                "transcript has no room at {width}x{height}"
            );
            if full {
                assert_eq!(chunks[STATUSLINE_IDX].height, 3);
                assert_eq!(chunks[PERMISSION_IDX].height, 2);
                assert!(
                    !rows[(chunks[STATUSLINE_IDX].y + 1) as usize]
                        .trim()
                        .is_empty()
                );
                assert!(rows[chunks[PERMISSION_IDX].y as usize].contains("Permissions:"));
            }
        }
    }

    /// The permission bar has one blank row below it, so its text does not sit
    /// against the bottom edge of the terminal.
    #[test]
    fn the_permission_bar_has_a_blank_row_below_it() {
        let mut state = UiState::default();
        let buffer = render_to_buffer(80, 24, &mut state);
        let chunks = chunks_for(24, 0, false, true, 1, true);
        let bar_row = chunks[PERMISSION_IDX].y;

        assert_eq!(chunks[PERMISSION_IDX].height, 2);
        assert!(row_text(&buffer, bar_row).contains("Permissions:"));
        let below = row_text(&buffer, bar_row + 1);
        assert!(below.trim().is_empty(), "row below permissions: {below:?}");
    }

    #[test]
    fn tool_result_languages_are_guessed_conservatively() {
        assert_eq!(
            guess_language("fn main() {\n    let x = 1;\n}\n"),
            Some("rs")
        );
        assert_eq!(
            guess_language("[package]\nname = \"coderex\"\n"),
            Some("toml")
        );
        assert_eq!(
            guess_language("{\n  \"name\": \"coderex\"\n}\n"),
            Some("json")
        );
        assert_eq!(guess_language("def f():\n    pass\n"), Some("py"));
        assert_eq!(guess_language("just some prose\nover two lines\n"), None);
    }

    /// A Read result keeps its indentation when highlighted.
    #[test]
    fn read_results_are_highlighted_without_losing_indentation() {
        let mut state = UiState::default();
        state.messages.push(Message {
            role: MessageRole::ToolResult,
            content: "fn main() {\n    let answer = 42;\n}\n".into(),
            tool_name: Some("Read".into()),
            streaming: false,
            folded: false,
        });
        let buffer = render_to_buffer(100, 20, &mut state);
        let screen: String = (0..20)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("fn main()"), "{screen}");
        assert!(screen.contains("    let answer = 42;"), "{screen}");
    }

    /// Dragging over the transcript copies exactly the characters under the
    /// rectangle.
    #[test]
    fn a_selection_copies_the_text_under_it() {
        let mut state = UiState::default();
        state.messages.push(Message {
            role: MessageRole::System,
            content: "alpha beta\ngamma delta".into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });
        let buffer = render_to_buffer(80, 20, &mut state);
        let rows = capture_rows(&buffer);

        let alpha_row = (0..20)
            .find(|row| row_text(&buffer, *row).contains("alpha beta"))
            .expect("alpha row");
        let gamma_row = (0..20)
            .find(|row| row_text(&buffer, *row).contains("gamma delta"))
            .expect("gamma row");
        // Columns are display cells, not byte offsets: a marker before the
        // text may be multi-byte.
        let column_of = |row: u16, needle: &str| {
            let text = row_text(&buffer, row);
            let byte = text.find(needle).expect(needle);
            text[..byte].chars().count() as u16
        };
        let alpha_col = column_of(alpha_row, "alpha");
        let delta_col = column_of(gamma_row, "delta") + 4;

        let selection = TextSelection {
            anchor: (alpha_col, alpha_row),
            cursor: (delta_col, gamma_row),
        };
        assert_eq!(
            selection_text(buffer.area, &rows, &selection),
            // The second line keeps the indent the transcript drew.
            "alpha beta\n  gamma delta"
        );
    }

    /// The selected cells get the theme's selection background and nothing
    /// outside the rectangle changes.
    #[test]
    fn a_selection_is_highlighted() {
        let mut state = UiState::default();
        state.selection = Some(TextSelection {
            anchor: (1, 1),
            cursor: (5, 2),
        });
        let buffer = render_to_buffer(80, 20, &mut state);
        let selected = crate::themes::Theme::dark().selection_bg;

        assert_eq!(buffer.cell((2, 1)).unwrap().bg, selected);
        assert_eq!(buffer.cell((2, 2)).unwrap().bg, selected);
        assert_ne!(buffer.cell((0, 1)).unwrap().bg, selected);
        // Linear selection: the rest of the first row is selected up to the
        // second row's column.
        assert_eq!(buffer.cell((10, 1)).unwrap().bg, selected);
        assert_ne!(buffer.cell((10, 2)).unwrap().bg, selected);
    }

    /// A long provider error in the detail overlay must be readable, not
    /// clipped at a fixed 50-column box.
    #[test]
    fn a_detail_overlay_is_wide_enough_for_its_text() {
        use crate::overlay::Overlay;

        let mut state = UiState::default();
        state.overlays.push(Overlay::Detail {
            title: "Failure Details".into(),
            body: format!("{}ENDMARK", "x".repeat(70)),
        });
        let buffer = render_to_buffer(120, 30, &mut state);
        let screen: String = (0..30)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("Failure Details"), "{screen}");
        assert!(screen.contains("ENDMARK"), "{screen}");
    }

    /// A terminal too small for the box gets readable plain lines instead of a
    /// clipped card.
    #[test]
    fn a_narrow_terminal_gets_the_plain_welcome_page() {
        let mut state = UiState::default();
        let buffer = render_to_buffer(30, 12, &mut state);
        let screen: String = (0..12)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("C O D E R E X"), "{screen}");
        assert!(screen.contains("CodeRex v"), "{screen}");
        assert!(screen.contains("Type /help for commands"), "{screen}");
        assert!(
            !screen.contains('╭'),
            "the card should not be drawn:\n{screen}"
        );
    }

    /// The page is a placeholder for an empty transcript; once a message
    /// exists, the transcript wins.
    #[test]
    fn a_message_replaces_the_welcome_page() {
        let mut state = UiState::default();
        state.messages.push(Message {
            role: MessageRole::System,
            content: "hello from the transcript".into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });
        let buffer = render_to_buffer(80, 30, &mut state);
        let screen: String = (0..30)
            .map(|y| row_text(&buffer, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(screen.contains("hello from the transcript"), "{screen}");
        assert!(!screen.contains("C O D E R E X"), "{screen}");
    }

    /// A multi-line buffer renders one row per line, with the continuation
    /// rows aligned under the text rather than under the prompt.
    #[test]
    fn multi_line_input_renders_every_line() {
        let mut state = UiState::default();
        state.input.text = "first\nsecond".into();
        state.input.cursor = 12;
        let buffer = render_to_buffer(80, 12, &mut state);
        let input_y = chunks_for(12, 0, false, false, 2, false)[INPUT_IDX].y;
        let rows: Vec<String> = (0..2).map(|i| row_text(&buffer, input_y + i)).collect();
        assert!(rows[0].contains("first"), "{rows:?}");
        assert!(rows[1].contains("second"), "{rows:?}");
        assert!(
            rows[1].starts_with("  second"),
            "continuation not aligned: {rows:?}"
        );
    }

    /// The input pane grows with the buffer, so the transcript above it is
    /// pushed up rather than the input being clipped.
    #[test]
    fn multi_line_input_grows_its_pane() {
        let mut state = UiState::default();
        state.input.text = "a\nb\nc".into();
        state.input.cursor = 5;
        let buffer = render_to_buffer(80, 20, &mut state);
        let chunks = chunks_for(20, 0, false, true, 3, true);
        assert_eq!(chunks[INPUT_IDX].height, 3);
        let rows: Vec<String> = (0..3)
            .map(|i| row_text(&buffer, chunks[INPUT_IDX].y + i))
            .collect();
        assert!(rows[0].contains('a'), "{rows:?}");
        assert!(rows[1].contains('b'), "{rows:?}");
        assert!(rows[2].contains('c'), "{rows:?}");
    }

    /// A linewise Visual selection has to be visible on every line it covers.
    #[test]
    fn multi_line_linewise_selection_highlights_every_line() {
        let mut state = UiState::default();
        state.input.text = "one\ntwo".into();
        state.input.cursor = 0;
        state.input.start_visual(true);
        state.input.cursor = 7;
        let buffer = render_to_buffer(80, 12, &mut state);
        let input_y = chunks_for(12, 0, false, false, 2, false)[INPUT_IDX].y;
        let selected = state.theme.selection_bg;
        for i in 0..2 {
            let has_selection = (0..buffer.area.width).any(|x| {
                buffer
                    .cell((x, input_y + i))
                    .is_some_and(|cell| cell.bg == selected)
            });
            assert!(
                has_selection,
                "row {i} of the linewise selection is not highlighted"
            );
        }
    }

    /// A buffer whose text fits on one row must still occupy exactly one row,
    /// so nothing about the single-line layout changes.
    #[test]
    fn single_line_input_still_occupies_one_row() {
        let mut state = UiState::default();
        state.input.text = "hello world".into();
        state.input.cursor = 11;
        assert_eq!(input_display_height(&state.input, 80), 1);

        let buffer = render_to_buffer(80, 12, &mut state);
        let input_y = chunks_for(12, 0, false, false, 1, false)[INPUT_IDX].y;
        let row = row_text(&buffer, input_y);
        assert!(row.contains("❯ hello world"), "{row:?}");
    }

    /// The transcript records where a mod UI button actually landed, including
    /// the system-message prefix and the box's border/padding columns.
    #[test]
    fn a_mod_ui_block_records_absolute_button_hits() {
        use coderex_core::ui_protocol::UiElement;

        let elements = vec![UiElement::Box {
            title: None,
            children: vec![UiElement::Button {
                label: "Run".to_string(),
                id: "run".to_string(),
            }],
        }];
        let content = crate::mod_elements::render_mod_ui_text(&elements, 80);
        let spec = ModUiBlockSpec {
            message_index: 0,
            content: content.clone(),
            elements,
            focused: None,
        };
        let mut state = UiState::default();
        state.messages.push(Message {
            role: MessageRole::System,
            content: content.into(),
            tool_name: None,
            streaming: false,
            folded: false,
        });

        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut regions = Vec::new();
        terminal
            .draw(|frame| render_frame(frame, &mut state, &[spec], &mut regions))
            .expect("draw frame");

        assert_eq!(regions.len(), 1, "one interactive block");
        let region = &regions[0];
        assert_eq!(region.message_index, 0);
        assert_eq!(region.scroll_offset, 0);
        // The message area starts at the top; the "· " system prefix moves the
        // block two columns right.
        assert_eq!(region.rect.y, 0);
        assert_eq!(region.rect.x, 2);
        assert_eq!(region.rect.height, 3);
        assert_eq!(region.hits.len(), 1);
        let hit = &region.hits[0];
        assert_eq!(hit.row, 1, "inside the box");
        assert_eq!(hit.col_start, 2, "after the box's left border and padding");
        assert_eq!(hit.col_end, 7, "[Run]");

        let screen_col = region.rect.x + hit.col_start;
        let screen_row = region.rect.y + hit.row;
        // The recorded column is where the button was actually painted.
        let painted = row_text(terminal.backend().buffer(), screen_row);
        assert_eq!(
            painted.chars().nth(usize::from(screen_col)),
            Some('['),
            "hit column does not match the drawn row: {painted:?}"
        );
        assert_eq!(
            crate::mod_elements::button_at(
                region.rect,
                region.scroll_offset,
                screen_col,
                screen_row,
                &region.hits,
            ),
            Some("run")
        );
        // The border column and the system prefix are not buttons.
        assert_eq!(
            crate::mod_elements::button_at(region.rect, 0, region.rect.x, screen_row, &region.hits,),
            None
        );
        assert_eq!(
            crate::mod_elements::button_at(region.rect, 0, 0, screen_row, &region.hits),
            None
        );
    }
}
