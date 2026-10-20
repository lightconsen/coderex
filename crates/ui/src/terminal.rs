//! Terminal UI rendering via ratatui.

use crate::message_list::{Message, MessageRole};
use crate::state::UiState;
use crate::statusline::Statusline;
use crate::terminal_profile::TerminalProfile;
use crate::themes::Theme;
use crossterm::event::{PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::queue;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::io::Stdout;
use std::sync::OnceLock;

/// Lazy-initialized syntax highlighter for the TUI.
fn syntax_highlighter() -> &'static coderex_core::syntax::SyntaxHighlighter {
    static HIGHLIGHTER: OnceLock<coderex_core::syntax::SyntaxHighlighter> = OnceLock::new();
    HIGHLIGHTER.get_or_init(coderex_core::syntax::SyntaxHighlighter::new)
}

/// ASCII T-Rex welcome art (larger variant).
const T_REX: &str = r"
                              .-.
                    ,        ( {o\
                    {\    /   ~o   }
                     \`  '-'/    ~ |
                    {.'   ~   ~  /}
                     `.    ~  ~ /
                    .'  ~  ~   `}.
                   /    ~     ~  `}
                  {      ~     ~  /)
                  |`-._  ~    ~  /
                  |    `-._     ~(
                  |        `--.__/)
                  {         `--._/
                   |  |  |  |   |
                   |  |  |  |   |
                   |  |  |  |   |
                   |  |  |  |   |
                   |  |  |  |   |
                   |__|__|__|___|
                   |__|__|__|___|
                   \____________/
                    \__________/
                     \________/
";

/// The main terminal UI.
#[allow(missing_debug_implementations)]
pub struct TerminalUi {
    terminal: ratatui::Terminal<CrosstermBackend<Stdout>>,
    #[allow(dead_code)]
    alternate: bool,
    profile: TerminalProfile,
    keyboard_pushed: bool,
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
        self.terminal.clear()?;
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
        self.terminal.draw(|frame| render_frame(frame, state))?;
        Ok(())
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
const INPUT_IDX: usize = 4;
const PERMISSION_IDX: usize = 5;

/// The vertical pane stack, top to bottom.
///
/// Shared with the layout tests so both sides agree on what each index means.
fn frame_constraints(
    inline_palette_height: u16,
    show_notif: bool,
    show_statusline: bool,
    show_perm_bar: bool,
) -> Vec<Constraint> {
    vec![
        Constraint::Min(1),                             // message area
        Constraint::Length(inline_palette_height),      // inline command hint
        Constraint::Length(u16::from(show_notif)),      // notification bar
        Constraint::Length(u16::from(show_statusline)), // statusline (thinking)
        Constraint::Length(1),                          // input line
        Constraint::Length(u16::from(show_perm_bar)),   // permission bar
    ]
}

/// Lay out and paint one frame.
///
/// Separate from `draw` so a `TestBackend` can drive it: the pane indices
/// below are the single place where layout order is decided, and getting one
/// wrong makes a pane render into the wrong rect without any error.
fn render_frame(frame: &mut Frame, state: &mut UiState) {
    let area = frame.area();
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(frame_constraints(
            inline_palette_height,
            show_notif,
            show_statusline,
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
        render_messages(frame, split_chunks[1], state);
    } else {
        render_messages(frame, msg_area, state);
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

    // Input line
    render_input(frame, chunks[INPUT_IDX], &state.input, &state.theme);

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

fn render_messages(frame: &mut Frame, area: ratatui::layout::Rect, state: &mut UiState) {
    let width = area.width as usize;
    let viewport_height = area.height as usize;
    let is_compact = area.height < 10;
    state.messages.viewport_height = viewport_height;

    // Clean up expired notifications
    state.notifications.cleanup();

    if state.messages.scroll_offset > 0 {
        let signature = message_render_signature(&state.messages.messages, &state.theme);
        let cache_valid = state.messages.render_cache.width == width
            && state.messages.render_cache.is_compact == is_compact
            && state.messages.render_cache.signature == signature;
        if !cache_valid {
            let mut lines =
                render_all_message_lines(&state.messages.messages, width, is_compact, &state.theme);
            if lines.is_empty() {
                lines.extend(render_welcome_art(&state.theme));
            }
            state.messages.render_cache.width = width;
            state.messages.render_cache.is_compact = is_compact;
            state.messages.render_cache.signature = signature;
            state.messages.render_cache.lines = lines;
        }
        state.messages.total_height = state.messages.render_cache.lines.len();
        let max_scroll = state
            .messages
            .render_cache
            .lines
            .len()
            .saturating_sub(viewport_height);
        state.messages.scroll_offset = state.messages.scroll_offset.min(max_scroll);
        let lines = slice_lines_to_viewport_with_scroll(
            &state.messages.render_cache.lines,
            viewport_height,
            state.messages.scroll_offset,
        );
        let paragraph = Paragraph::new(lines).block(Block::default().borders(Borders::NONE));
        frame.render_widget(paragraph, area);
        return;
    }

    // Only the bottom/non-cached path needs per-message heights. The scrollback
    // path above slices cached rendered lines directly, keeping wheel/PageUp
    // redraws independent of transcript size.
    let mut heights: Vec<usize> = Vec::with_capacity(state.messages.messages.len());
    for msg in &state.messages.messages {
        if msg.folded {
            heights.push(1);
        } else {
            heights.push(message_display_height(msg, width));
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

    for msg in visible {
        append_message_lines(&mut lines, msg, width, is_compact, &state.theme);
    }

    // If no messages, show dynamic welcome art
    if lines.is_empty() {
        lines.extend(render_welcome_art(&state.theme));
    }

    state.messages.total_height = lines.len();
    lines = trim_lines_to_viewport(lines, viewport_height);

    let paragraph = Paragraph::new(lines).block(Block::default().borders(Borders::NONE));
    frame.render_widget(paragraph, area);
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
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for msg in messages {
        append_message_lines(&mut lines, msg, width, is_compact, theme);
    }
    lines
}

fn message_render_signature(messages: &[Message], theme: &Theme) -> u64 {
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
    signature
}

fn append_message_lines(
    lines: &mut Vec<Line<'static>>,
    msg: &Message,
    width: usize,
    is_compact: bool,
    theme: &Theme,
) {
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
        return;
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
        return;
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

    // Show tool name for tool messages
    if let (Some(tool), MessageRole::ToolCall | MessageRole::ToolResult) =
        (&msg.tool_name, msg.role)
    {
        lines.push(Line::from(Span::styled(
            format!("  [{tool}]"),
            Style::default().fg(theme.dim),
        )));
    }
}

fn trim_lines_to_viewport(mut lines: Vec<Line>, viewport_height: usize) -> Vec<Line> {
    if viewport_height == 0 || lines.len() <= viewport_height {
        return lines;
    }
    lines.split_off(lines.len() - viewport_height)
}

fn slice_lines_to_viewport_with_scroll(
    lines: &[Line<'static>],
    viewport_height: usize,
    scroll_offset: usize,
) -> Vec<Line<'static>> {
    if viewport_height == 0 || lines.len() <= viewport_height {
        return lines.to_vec();
    }

    let end = lines
        .len()
        .saturating_sub(scroll_offset)
        .max(viewport_height);
    let start = end.saturating_sub(viewport_height);
    lines[start..end.min(lines.len())].to_vec()
}

/// Dynamic welcome art — cycles between T-Rex ASCII, tips, and hints.
fn render_welcome_art(theme: &Theme) -> Vec<Line<'static>> {
    const TIPS: &[&str] = &[
        "  Tip: Press / for commands, Ctrl+C to interrupt.",
        "  Tip: Type /vim to enable Vim keybindings.",
        "  Tip: Use Ctrl+F to search messages.",
        "  Tip: Press Ctrl+P for the command palette.",
        "  Tip: Type /help to see all available commands.",
    ];

    let mut lines: Vec<Line> = Vec::new();

    // Show T-Rex on a rotating basis (using a simple hash of tips count)
    // For simplicity, always show T-Rex with a random tip below it
    // In a real implementation, you'd track frame count for rotation
    for line in T_REX.lines() {
        lines.push(Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(theme.assistant),
        )));
    }
    lines.push(Line::from(Span::raw("")));

    // Show a tip
    let tip = TIPS[0]; // In production, rotate based on frame count
    lines.push(Line::from(Span::styled(
        tip.to_string(),
        Style::default().fg(theme.dim),
    )));

    lines.push(Line::from(Span::styled(
        "  Welcome to CodeRex. Type a message and press Enter.".to_string(),
        Style::default().fg(theme.dim),
    )));

    lines
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
    let rendered = statusline.render(area.width as usize);
    let spans = rendered
        .split(" | ")
        .map(|s| Span::raw(s.to_string()))
        .collect::<Vec<_>>();
    let line = Line::from(spans);
    let paragraph = Paragraph::new(line)
        .style(Style::default().fg(theme.statusbar_fg))
        .alignment(Alignment::Left);
    frame.render_widget(paragraph, area);
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

fn render_input(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    input: &crate::input::InputBuffer,
    theme: &Theme,
) {
    // Byte offsets are not columns: a CJK character occupies two cells. Place
    // the terminal cursor by display width so it lines up while selecting.
    let cursor = input.cursor_display_column();
    let text_style = Style::default().fg(theme.input_fg);
    let mut spans: Vec<Span> = vec![Span::styled("❯ ", Style::default().fg(theme.accent))];
    match input.selection_range() {
        Some((start, end)) if start < end => {
            if start > 0 {
                spans.push(Span::styled(input.text[..start].to_string(), text_style));
            }
            spans.push(Span::styled(
                input.text[start..end].to_string(),
                Style::default()
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg),
            ));
            if end < input.text.len() {
                spans.push(Span::styled(input.text[end..].to_string(), text_style));
            }
        }
        _ => spans.push(Span::styled(input.text.clone(), text_style)),
    }
    if let Some(ref sug) = input.suggestion {
        spans.push(Span::styled(
            format!("  {sug}"),
            Style::default().fg(theme.dim),
        ));
    }
    let line = Line::from(spans);
    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, area);
    // Position cursor
    frame.set_cursor_position((
        area.x + 2 + u16::try_from(cursor).unwrap_or(u16::MAX),
        area.y,
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
    let paragraph = Paragraph::new(content)
        .style(Style::default().fg(state.theme.foreground))
        .block(block);

    // Center the overlay
    let w = 50.min(area.width - 4);
    let h = 10.min(area.height - 4);
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
        let lines = vec![
            Line::from("line 1"),
            Line::from("line 2"),
            Line::from("line 3"),
            Line::from("line 4"),
            Line::from("line 5"),
        ];
        let trimmed = slice_lines_to_viewport_with_scroll(&lines, 2, 2);

        assert_eq!(trimmed.len(), 2);
        assert_eq!(trimmed[0].to_string(), "line 2");
        assert_eq!(trimmed[1].to_string(), "line 3");
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
        let lines = render_all_message_lines(&messages, 80, false, &theme);

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
            .draw(|frame| render_frame(frame, state))
            .expect("draw frame");
        terminal.backend().buffer().clone()
    }

    /// The pane stack the renderer lays out, for the given visibility flags.
    fn chunks_for(
        height: u16,
        palette: u16,
        notif: bool,
        statusline: bool,
        perm: bool,
    ) -> Vec<ratatui::layout::Rect> {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints(frame_constraints(palette, notif, statusline, perm))
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

        let chunks = chunks_for(30, 0, false, true, true);
        let statusline_y = chunks[STATUSLINE_IDX].y;
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

        let chunks = chunks_for(12, 0, false, false, false);
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

    /// Every index the renderer uses must name the constraint it claims to.
    #[test]
    fn pane_indices_follow_the_constraint_order() {
        let chunks = chunks_for(30, 4, true, true, true);
        assert_eq!(chunks.len(), PERMISSION_IDX + 1, "one rect per constraint");
        assert_eq!(chunks[PALETTE_IDX].height, 4, "palette");
        assert_eq!(chunks[NOTIFICATION_IDX].height, 1, "notification");
        assert_eq!(chunks[STATUSLINE_IDX].height, 1, "statusline");
        assert_eq!(chunks[INPUT_IDX].height, 1, "input");
        assert_eq!(chunks[PERMISSION_IDX].height, 1, "permission");
        assert!(chunks[0].height > 1, "message area");
        // Rects are contiguous and in order, top to bottom.
        for pair in chunks.windows(2) {
            assert_eq!(pair[0].y + pair[0].height, pair[1].y);
        }
    }
}
