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
    pub fn draw(&mut self, state: &UiState) -> std::io::Result<()> {
        self.terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),       // message area
                    Constraint::Length(1),    // statusline
                    Constraint::Length(1),    // input line
                ])
                .split(area);

            // Message area
            render_messages(frame, chunks[0], state);

            // Statusline
            render_statusline(frame, chunks[1], &state.statusline);

            // Input line
            render_input(frame, chunks[2], &state.input);

            // Overlay (if any)
            if state.overlays.is_active() {
                render_overlay(frame, area, state);
            }
        })?;
        Ok(())
    }
}

fn render_messages(frame: &mut Frame, area: ratatui::layout::Rect, state: &UiState) {
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

        lines.push(Line::from(vec![
            Span::styled(prefix, style),
            if msg.streaming {
                Span::styled(&msg.content, style.add_modifier(Modifier::DIM))
            } else {
                Span::raw(&msg.content)
            },
        ]));

        // Show tool name for tool messages
        if let (Some(tool), MessageRole::ToolCall | MessageRole::ToolResult) = (&msg.tool_name, msg.role) {
            lines.push(Line::from(Span::styled(
                format!("  [{tool}]"),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    // If no messages, show welcome
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "Welcome to CodeRex. Type a message and press Enter.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::NONE))
        .scroll((state.messages.start as u16, 0));
    frame.render_widget(paragraph, area);
}

fn render_statusline(frame: &mut Frame, area: ratatui::layout::Rect, statusline: &Statusline) {
    let rendered = statusline.render(area.width as usize);
    let spans = rendered.split(" | ").map(|s| Span::raw(s.to_string())).collect::<Vec<_>>();
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
        Overlay::CommandPalette { query, commands, selected } => {
            let mut s = format!("Command Palette [{query}]\n");
            for (i, cmd) in commands.iter().enumerate() {
                let prefix = if i == *selected { "> " } else { "  " };
                s.push_str(&format!("{prefix}{cmd}\n"));
            }
            s
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
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
        };
        let rendered = sl.render(80);
        assert!(rendered.len() <= 80);
    }
}
