//! UI state — separate from business kernel for immediate-mode rendering.

/// The active view mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// Main REPL view.
    MainRepl,
    /// Permission modal view.
    PermissionModal,
    /// Message selector view.
    MessageSelector,
    /// Command palette view.
    CommandPalette,
    /// Help overlay view.
    HelpOverlay,
}

/// Type of overlay displayed on top of the main UI.
#[derive(Debug, Clone)]
pub enum OverlayType {
    /// Permission dialog overlay.
    PermissionDialog {
        /// The tool name requesting permission.
        tool: String,
        /// The arguments passed to the tool.
        args: String,
    },
    /// Notification overlay.
    Notification {
        /// The notification message.
        message: String,
    },
    /// Command palette overlay.
    CommandPalette {
        /// The current search query.
        query: String,
    },
}

/// A notification to display.
#[derive(Debug, Clone)]
pub struct Notification {
    /// The notification message text.
    pub message: String,
}

/// UI state, owned exclusively by the rendering thread.
#[derive(Debug)]
pub struct UiState {
    /// Currently active view.
    pub active_view: ViewMode,
    /// User input buffer.
    pub input_buffer: String,
    /// Cursor position in the input buffer.
    pub cursor_position: usize,
    /// Active overlay layers.
    pub overlays: Vec<OverlayType>,
    /// Vertical scroll offset.
    pub scroll_offset: usize,
    /// Pending notifications.
    pub notifications: Vec<Notification>,
    /// Companion/buddy state.
    pub companion: Option<CompanionState>,
}

/// Companion/buddy state.
#[derive(Debug, Clone)]
pub struct CompanionState {
    /// The companion sprite identifier.
    pub sprite: String,
    /// The companion's current mood.
    pub mood: String,
}

impl UiState {
    /// Create a new UiState with defaults.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn new() -> Self {
        Self {
            active_view: ViewMode::MainRepl,
            input_buffer: String::new(),
            cursor_position: 0,
            overlays: Vec::new(),
            scroll_offset: 0,
            notifications: Vec::new(),
            companion: None,
        }
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}
