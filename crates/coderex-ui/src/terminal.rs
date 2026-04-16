//! Terminal UI rendering via ratatui.

use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use std::io::Stdout;
use crate::state::UiState;

/// The main terminal UI.
// `ratatui::Terminal` does not implement Debug, so we cannot derive it.
#[allow(missing_debug_implementations)]
pub struct TerminalUi {
    terminal: ratatui::Terminal<CrosstermBackend<Stdout>>,
    #[allow(dead_code)]
    ui_state: UiState,
}

impl TerminalUi {
    /// Create a new terminal UI.
    pub fn new() -> std::io::Result<Self> {
        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self {
            terminal,
            ui_state: UiState::new(),
        })
    }

    /// Draw a single frame.
    pub fn draw<F>(&mut self, _render_fn: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut Frame, &UiState),
    {
        // TODO: implement immediate-mode rendering tree
        Ok(())
    }

    /// Enter alternate screen.
    pub fn enter(&mut self) -> std::io::Result<()> {
        self.terminal.clear()?;
        Ok(())
    }

    /// Leave alternate screen.
    #[allow(clippy::missing_const_for_fn)]
    pub fn exit(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
