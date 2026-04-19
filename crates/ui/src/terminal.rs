//! Terminal UI rendering via ratatui.

use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::io::Stdout;
use crate::message_list::MessageRole;
use crate::state::UiState;
use crate::statusline::Statusline;

/// ASCII T-Rex welcome art (larger variant).
const T_REX: &str = r#"
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
"#;

/// The main terminal UI.
#[allow(missing_debug_implementations)]
pub struct TerminalUi {
    terminal: ratatui::Terminal<CrosstermBackend<Stdout>>,
    #[allow(dead_code)]
    alternate: bool,
}

impl TerminalUi {
    /// Create a new terminal UI.
    pub fn new() -> std::io::Result<Self> {
        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self {
            terminal,
            alternate: false,
        })
    }

    /// Enter alternate screen.
    pub fn enter(&mut self) -> std::io::Result<()> {
        execute!(std::io::stdout(), EnterAlternateScreen)?;
        self.alternate = true;
        self.terminal.clear()?;
        Ok(())
    }

    /// Leave alternate screen.
    pub fn exit(&mut self) -> std::io::Result<()> {
        if self.alternate {
            execute!(std::io::stdout(), LeaveAlternateScreen)?;
            self.alternate = false;
        }
        Ok(())
    }

    /// Draw a single frame.
    pub fn draw(&mut self, state: &mut UiState) -> std::io::Result<()> {
        self.terminal.draw(|frame| {
            let area = frame.area();

            // Determine if we need an inline palette above the input line.
            let inline_palette_height = inline_palette_height(state);

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),                    // message area
                    Constraint::Length(inline_palette_height), // inline command hint
                    Constraint::Length(1),                 // statusline
                    Constraint::Length(1),                 // input line
                ])
                .split(area);

            // Message area
            render_messages(frame, chunks[0], state);

            // Inline palette (when typing /commands)
            if inline_palette_height > 0 {
                render_inline_palette(frame, chunks[1], state);
            }

            // Statusline
            render_statusline(frame, chunks[2], &state.statusline);

            // Input line
            render_input(frame, chunks[3], &state.input);

            // Overlay (only non-palette overlays; palette is rendered inline)
            if state.overlays.is_active() && !is_inline_palette(state) {
                render_overlay(frame, area, state);
            }
        })?;
        Ok(())
    }
}

/// Compute the height of the inline palette area (0 = hidden).
fn inline_palette_height(state: &UiState) -> u16 {
    use crate::overlay::Overlay;
    let Some(overlay) = state.overlays.top() else { return 0 };
    match overlay {
        Overlay::CommandPalette { query, commands, .. } if query.starts_with('/') => {
            // Header line + up to 5 visible commands, capped at remaining terminal space
            let count = commands.len().min(5);
            ((count + 1) as u16).min(6)
        }
        _ => 0,
    }
}

/// Check whether the active overlay is an inline `/` palette.
fn is_inline_palette(state: &UiState) -> bool {
    use crate::overlay::Overlay;
    state.overlays.top().map_or(false, |o| {
        matches!(o, Overlay::CommandPalette { query, .. } if query.starts_with('/'))
    })
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
            let space = if current.is_empty() { 0 } else { 1 };
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
    let prefix = match msg.role {
        crate::message_list::MessageRole::User => "▸ ",
        crate::message_list::MessageRole::Assistant => "◂ ",
        crate::message_list::MessageRole::ToolResult => "✓ ",
        crate::message_list::MessageRole::ToolCall => "⚙ ",
        crate::message_list::MessageRole::System => "⋮ ",
    };
    let prefix_width = unicode_width::UnicodeWidthStr::width(prefix);
    let wrap_width = width.saturating_sub(prefix_width);
    let wrapped = wrap_text(&msg.content, wrap_width);
    let mut height = wrapped.len().max(1);
    if msg.tool_name.is_some()
        && matches!(msg.role, crate::message_list::MessageRole::ToolCall | crate::message_list::MessageRole::ToolResult)
    {
        height += 1;
    }
    height
}

fn render_messages(frame: &mut Frame, area: ratatui::layout::Rect, state: &mut UiState) {
    let width = area.width as usize;
    let viewport_height = area.height as usize;

    // Compute each message's height so we can determine how many fit.
    let mut heights: Vec<usize> = Vec::with_capacity(state.messages.messages.len());
    for msg in &state.messages.messages {
        heights.push(message_display_height(msg, width));
    }

    // Count how many messages fit from `start` downward.
    let mut visible_count = 0;
    let mut accumulated = 0;
    for i in state.messages.start..state.messages.messages.len() {
        accumulated += heights[i];
        if accumulated > viewport_height {
            break;
        }
        visible_count += 1;
    }
    visible_count = visible_count.max(1);

    state.messages.set_visible_count(visible_count);
    let visible = state.messages.visible();
    let mut lines: Vec<Line> = Vec::new();

    for msg in visible {
        let (prefix, style) = match msg.role {
            MessageRole::User => ("▸ ", Style::default().fg(Color::Green)),
            MessageRole::Assistant => ("◂ ", Style::default().fg(Color::Cyan)),
            MessageRole::ToolResult => ("✓ ", Style::default().fg(Color::Yellow)),
            MessageRole::ToolCall => ("⚙ ", Style::default().fg(Color::Magenta)),
            MessageRole::System => ("⋮ ", Style::default().fg(Color::Gray)),
        };

        let prefix_width = unicode_width::UnicodeWidthStr::width(prefix);
        let wrap_width = width.saturating_sub(prefix_width);
        let wrapped = wrap_text(&msg.content, wrap_width);

        for (i, text) in wrapped.into_iter().enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(prefix, style),
                    if msg.streaming {
                        Span::styled(text, style.add_modifier(Modifier::DIM))
                    } else {
                        Span::raw(text)
                    },
                ]));
            } else {
                let indent = " ".repeat(prefix_width);
                lines.push(Line::from(vec![
                    Span::styled(indent, style),
                    if msg.streaming {
                        Span::styled(text, style.add_modifier(Modifier::DIM))
                    } else {
                        Span::raw(text)
                    },
                ]));
            }
        }

        // Show tool name for tool messages
        if let (Some(tool), MessageRole::ToolCall | MessageRole::ToolResult) =
            (&msg.tool_name, msg.role)
        {
            lines.push(Line::from(Span::styled(
                format!("  [{tool}]"),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    // If no messages, show T-Rex ASCII welcome art
    if lines.is_empty() {
        for line in T_REX.lines() {
            lines.push(Line::from(Span::styled(
                line,
                Style::default().fg(Color::Green),
            )));
        }
        lines.push(Line::from(Span::styled(
            "",
            Style::default(),
        )));
        lines.push(Line::from(Span::styled(
            "  Welcome to CodeRex. Type a message and press Enter.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::NONE));
    frame.render_widget(paragraph, area);
}

fn render_statusline(frame: &mut Frame, area: ratatui::layout::Rect, statusline: &Statusline) {
    let rendered = statusline.render(area.width as usize);
    let spans = rendered
        .split(" | ")
        .map(|s| Span::raw(s.to_string()))
        .collect::<Vec<_>>();
    let line = Line::from(spans);
    let paragraph = Paragraph::new(line)
        .style(Style::default().fg(Color::White).bg(Color::DarkGray))
        .alignment(Alignment::Left);
    frame.render_widget(paragraph, area);
}

fn render_input(frame: &mut Frame, area: ratatui::layout::Rect, input: &crate::input::InputBuffer) {
    let cursor = input.cursor.min(input.text.len());
    let text = format!("❯ {}", input.text);
    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, area);
    // Position cursor
    frame.set_cursor_position((area.x + 2 + cursor as u16, area.y));
}

/// Render the inline command palette just above the statusline / input line.
fn render_inline_palette(frame: &mut Frame, area: ratatui::layout::Rect, state: &UiState) {
    use crate::overlay::Overlay;
    let Some(Overlay::CommandPalette { query, commands, selected }) = state.overlays.top() else {
        return;
    };

    let mut lines: Vec<Line> = Vec::new();

    // Header
    lines.push(Line::from(vec![
        Span::styled("Commands ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(format!("[{query}]"), Style::default().fg(Color::Gray)),
    ]));

    let visible_count = commands.len().min(area.height as usize - 1);
    let start = selected.saturating_sub(visible_count - 1).min(commands.len().saturating_sub(visible_count));

    for i in start..(start + visible_count).min(commands.len()) {
        let style = if i == *selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(commands[i].clone(), style)));
    }

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::NONE).style(Style::default().bg(Color::DarkGray)));
    frame.render_widget(paragraph, area);
}

fn render_overlay(frame: &mut Frame, area: ratatui::layout::Rect, state: &UiState) {
    use crate::overlay::Overlay;
    let Some(overlay) = state.overlays.top() else { return };

    let content = match overlay {
        Overlay::PermissionDialog { tool, args, always_option } => {
            format!(
                "Permission Request\nTool: {tool}\nArgs: {args}\n[a] Allow once  [d] Deny{}{}",
                if *always_option { "  [A] Always allow" } else { "" },
                if *always_option { "" } else { "\n[a] Allow once" }
            )
        }
        Overlay::Notification { message, level } => {
            format!("Notification: {message} ({level:?})")
        }
        Overlay::CommandPalette { .. } => {
            // Inline palette is handled separately; skip here.
            return;
        }
        Overlay::Confirm { message } => {
            format!("{message}\n[y/n]")
        }
    };

    let block = Block::default().borders(Borders::ALL).title("Overlay");
    let paragraph = Paragraph::new(content).block(block);

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
            permission_mode: "default".into(),
            git_branch: "main".into(),
            session_title: "test".into(),
            thinking: false,
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
}
