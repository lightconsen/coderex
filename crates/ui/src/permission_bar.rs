//! Permission bar — rendered below the input line, shows current permission mode.

use coderex_core::permissions::PermissionMode;
use ratatui::style::Color;

/// Permission bar data to render below the input line.
#[derive(Debug, Clone)]
pub struct PermissionBar {
    /// Current permission mode.
    pub mode: PermissionMode,
}

impl PermissionBar {
    /// Create a new permission bar with the default mode.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mode: PermissionMode::default(),
        }
    }

    /// Cycle to the next permission mode (Shift+Tab behaviour).
    pub fn cycle_mode(&mut self) {
        self.mode = match self.mode {
            PermissionMode::Default => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::BypassPermissions,
            PermissionMode::BypassPermissions => PermissionMode::DontAsk,
            PermissionMode::DontAsk => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::Auto,
            PermissionMode::Auto => PermissionMode::Default,
        };
    }

    /// Human-readable label for the current mode.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self.mode {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "accept-edits",
            PermissionMode::BypassPermissions => "bypass-permissions",
            PermissionMode::DontAsk => "dont-ask",
            PermissionMode::Plan => "plan",
            PermissionMode::Auto => "auto",
        }
    }

    /// Short label (used when width is tight).
    #[must_use]
    pub fn short_label(&self) -> &'static str {
        match self.mode {
            PermissionMode::Default => "def",
            PermissionMode::AcceptEdits => "edits",
            PermissionMode::BypassPermissions => "bypass",
            PermissionMode::DontAsk => "deny",
            PermissionMode::Plan => "plan",
            PermissionMode::Auto => "auto",
        }
    }

    /// Foreground colour for the current mode.
    #[must_use]
    pub fn color(&self) -> Color {
        match self.mode {
            PermissionMode::Default => Color::Yellow,
            PermissionMode::AcceptEdits => Color::Green,
            PermissionMode::BypassPermissions => Color::Red,
            PermissionMode::DontAsk => Color::Magenta,
            PermissionMode::Plan => Color::Cyan,
            PermissionMode::Auto => Color::Blue,
        }
    }

    /// Render the permission bar to a single-line string.
    #[must_use]
    pub fn render(&self, width: usize) -> String {
        let label = self.label();
        let hint = "Shift+Tab to cycle";
        let text = format!("Permissions: {label}  ({hint})");
        let text_width = unicode_width::UnicodeWidthStr::width(text.as_str());
        if text_width <= width {
            let pad = width.saturating_sub(text_width);
            format!("{text}{}", " ".repeat(pad))
        } else {
            let short = self.short_label();
            let text = format!("Permissions: {short}");
            let text_width = unicode_width::UnicodeWidthStr::width(text.as_str());
            let pad = width.saturating_sub(text_width);
            format!("{text}{}", " ".repeat(pad))
        }
    }
}

impl Default for PermissionBar {
    fn default() -> Self {
        Self::new()
    }
}
