//! Permission bar — rendered below the input line, shows current permission mode.

use coderex_core::permissions::PermissionMode;
use ratatui::style::Color;

use crate::themes::Theme;

/// Permission bar data to render below the input line.
#[derive(Debug, Clone)]
pub struct PermissionBar {
    /// Current permission mode.
    pub mode: PermissionMode,
    /// Current permission status shown to the user.
    pub status: Option<PermissionStatus>,
}

/// Current interactive permission state.
#[derive(Debug, Clone)]
pub struct PermissionStatus {
    /// Status kind.
    pub kind: PermissionStatusKind,
    /// Short user-facing label.
    pub label: String,
}

/// Kinds of permission status shown in the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStatusKind {
    /// Waiting for user approval.
    Pending,
    /// The user approved the request.
    Approved,
    /// The user denied the request.
    Denied,
}

impl PermissionBar {
    /// Create a new permission bar with the default mode.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mode: PermissionMode::default(),
            status: None,
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
    pub fn color(&self, theme: &Theme) -> Color {
        if let Some(status) = &self.status {
            return match status.kind {
                PermissionStatusKind::Pending => theme.warning,
                PermissionStatusKind::Approved => theme.success,
                PermissionStatusKind::Denied => theme.error,
            };
        }

        match self.mode {
            PermissionMode::Default => theme.warning,
            PermissionMode::AcceptEdits => theme.success,
            PermissionMode::BypassPermissions => theme.error,
            PermissionMode::DontAsk => theme.tool,
            PermissionMode::Plan => theme.accent_secondary,
            PermissionMode::Auto => theme.accent,
        }
    }

    /// Show that a permission request is blocking execution.
    pub fn set_pending(&mut self, label: impl Into<String>) {
        self.status = Some(PermissionStatus {
            kind: PermissionStatusKind::Pending,
            label: label.into(),
        });
    }

    /// Show the last approval/denial result.
    pub fn set_resolution(&mut self, approved: bool, label: impl Into<String>) {
        self.status = Some(PermissionStatus {
            kind: if approved {
                PermissionStatusKind::Approved
            } else {
                PermissionStatusKind::Denied
            },
            label: label.into(),
        });
    }

    /// Clear the blocking permission state while preserving resolved status.
    pub fn clear_pending(&mut self) {
        if matches!(
            self.status.as_ref().map(|status| status.kind),
            Some(PermissionStatusKind::Pending)
        ) {
            self.status = None;
        }
    }

    /// Clear any rendered permission status.
    pub fn clear_status(&mut self) {
        self.status = None;
    }

    /// Render the permission bar to a single-line string.
    #[must_use]
    pub fn render(&self, width: usize) -> String {
        let label = self.label();
        let hint = "Shift+Tab to cycle";
        let status = self
            .status
            .as_ref()
            .map(|status| match status.kind {
                PermissionStatusKind::Pending => format!(" | waiting: {}", status.label),
                PermissionStatusKind::Approved => format!(" | approved: {}", status.label),
                PermissionStatusKind::Denied => format!(" | denied: {}", status.label),
            })
            .unwrap_or_default();
        let text = format!("Permissions: {label}{status}  ({hint})");
        let text_width = unicode_width::UnicodeWidthStr::width(text.as_str());
        if text_width <= width {
            let pad = width.saturating_sub(text_width);
            format!("{text}{}", " ".repeat(pad))
        } else {
            let short = self.short_label();
            let text = if let Some(status) = &self.status {
                let short_status = match status.kind {
                    PermissionStatusKind::Pending => "wait",
                    PermissionStatusKind::Approved => "ok",
                    PermissionStatusKind::Denied => "deny",
                };
                format!("Permissions: {short} | {short_status}")
            } else {
                format!("Permissions: {short}")
            };
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
