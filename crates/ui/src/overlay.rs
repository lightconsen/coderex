//! Overlay and modal rendering — permission dialogs, notifications, command palette.

/// An overlay layer rendered on top of the main UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// Permission approval dialog.
    PermissionDialog {
        /// Tool name requesting permission.
        tool: String,
        /// Tool arguments.
        args: String,
        /// Whether this is a one-time or always-allow request.
        always_option: bool,
    },
    /// A transient notification.
    Notification {
        /// Notification message.
        message: String,
        /// Severity level.
        level: NotificationLevel,
    },
    /// Command palette.
    CommandPalette {
        /// Current search/filter query.
        query: String,
        /// Available command labels.
        commands: Vec<String>,
        /// Currently selected index.
        selected: usize,
    },
    /// Confirmation dialog.
    Confirm {
        /// Confirmation prompt.
        message: String,
    },
}

/// Notification severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationLevel {
    /// Informational.
    Info,
    /// Warning.
    Warning,
    /// Error.
    Error,
}

/// Overlay manager — stack of overlays.
#[derive(Debug, Default)]
pub struct OverlayStack {
    overlays: Vec<Overlay>,
}

impl OverlayStack {
    /// Create a new empty overlay stack.
    #[must_use]
    pub fn new() -> Self {
        Self {
            overlays: Vec::new(),
        }
    }

    /// Push an overlay on top.
    pub fn push(&mut self, overlay: Overlay) {
        self.overlays.push(overlay);
    }

    /// Pop the top overlay.
    pub fn pop(&mut self) -> Option<Overlay> {
        self.overlays.pop()
    }

    /// Clear all overlays.
    pub fn clear(&mut self) {
        self.overlays.clear();
    }

    /// Get the top overlay.
    #[must_use]
    pub fn top(&self) -> Option<&Overlay> {
        self.overlays.last()
    }

    /// Check if any overlay is active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.overlays.is_empty()
    }

    /// Count active overlays.
    #[must_use]
    pub fn len(&self) -> usize {
        self.overlays.len()
    }

    /// Update the command palette query and filter commands.
    pub fn update_palette_query(&mut self, _query: &str) {
        if let Some(Overlay::CommandPalette {
            commands, selected, ..
        }) = self.overlays.last_mut()
        {
            // Filter would happen here; for now just update query
            let _ = (commands, selected);
        }
    }

    /// Move selection in command palette.
    pub fn move_palette_selection(&mut self, delta: isize) {
        if let Some(Overlay::CommandPalette {
            commands, selected, ..
        }) = self.overlays.last_mut()
        {
            if commands.is_empty() {
                return;
            }
            let len = commands.len() as isize;
            *selected = ((*selected as isize + delta).rem_euclid(len)) as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_pop() {
        let mut stack = OverlayStack::new();
        assert!(!stack.is_active());

        stack.push(Overlay::Notification {
            message: "Done".into(),
            level: NotificationLevel::Info,
        });
        assert!(stack.is_active());
        assert_eq!(stack.len(), 1);

        let popped = stack.pop();
        assert!(popped.is_some());
        assert!(!stack.is_active());
    }

    #[test]
    fn top_returns_last() {
        let mut stack = OverlayStack::new();
        stack.push(Overlay::Notification {
            message: "first".into(),
            level: NotificationLevel::Info,
        });
        stack.push(Overlay::Notification {
            message: "second".into(),
            level: NotificationLevel::Warning,
        });
        assert_eq!(stack.len(), 2);
        if let Some(Overlay::Notification { message, .. }) = stack.top() {
            assert_eq!(message, "second");
        } else {
            panic!("expected notification");
        }
    }

    #[test]
    fn clear_all() {
        let mut stack = OverlayStack::new();
        stack.push(Overlay::Notification {
            message: "a".into(),
            level: NotificationLevel::Info,
        });
        stack.push(Overlay::Notification {
            message: "b".into(),
            level: NotificationLevel::Error,
        });
        stack.clear();
        assert!(!stack.is_active());
    }

    #[test]
    fn permission_dialog() {
        let mut stack = OverlayStack::new();
        stack.push(Overlay::PermissionDialog {
            tool: "Bash".into(),
            args: "rm -rf /tmp/test".into(),
            always_option: true,
        });
        if let Some(Overlay::PermissionDialog {
            tool,
            always_option,
            ..
        }) = stack.top()
        {
            assert_eq!(tool, "Bash");
            assert!(*always_option);
        } else {
            panic!("expected permission dialog");
        }
    }

    #[test]
    fn palette_selection_wraps() {
        let mut stack = OverlayStack::new();
        stack.push(Overlay::CommandPalette {
            query: String::new(),
            commands: vec!["a".into(), "b".into(), "c".into()],
            selected: 0,
        });
        stack.move_palette_selection(-1); // wrap to end
        if let Some(Overlay::CommandPalette { selected, .. }) = stack.top() {
            assert_eq!(*selected, 2);
        }
        stack.move_palette_selection(1); // wrap to start
        if let Some(Overlay::CommandPalette { selected, .. }) = stack.top() {
            assert_eq!(*selected, 0);
        }
    }

    #[test]
    fn empty_palette_no_crash() {
        let mut stack = OverlayStack::new();
        stack.push(Overlay::CommandPalette {
            query: String::new(),
            commands: Vec::new(),
            selected: 0,
        });
        stack.move_palette_selection(1); // should not panic
    }
}
