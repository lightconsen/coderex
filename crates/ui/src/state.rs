//! UI state — separate from business kernel for immediate-mode rendering.

use crate::input::InputBuffer;
use crate::message_list::MessageView;
use crate::overlay::OverlayStack;
use crate::search::SearchState;
use crate::selector::MessageSelector;
use crate::statusline::Statusline;
use crate::statusline_setup::StatuslineSetup;

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
    /// Search mode.
    Search,
    /// Voice recording mode.
    #[cfg(feature = "voice")]
    VoiceRecording,
    /// Buddy companion panel.
    #[cfg(feature = "buddy")]
    BuddyPanel,
    /// Statusline setup wizard.
    StatuslineSetup,
}

/// UI state, owned exclusively by the rendering thread.
#[derive(Debug)]
pub struct UiState {
    /// Currently active view.
    pub active_view: ViewMode,
    /// Input buffer for user typing.
    pub input: InputBuffer,
    /// Message list with virtualization.
    pub messages: MessageView,
    /// Overlay stack.
    pub overlays: OverlayStack,
    /// Search state.
    pub search: SearchState,
    /// Message selector (for multi-select mode).
    pub selector: Option<MessageSelector>,
    /// Statusline data.
    pub statusline: Statusline,
    /// Statusline setup wizard state.
    pub statusline_setup: StatuslineSetup,
}

impl UiState {
    /// Create a new UiState with defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            active_view: ViewMode::MainRepl,
            input: InputBuffer::new(),
            messages: MessageView::new(1000),
            overlays: OverlayStack::new(),
            search: SearchState::new(),
            selector: None,
            statusline: Statusline::new(),
            statusline_setup: StatuslineSetup::new(),
        }
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}
