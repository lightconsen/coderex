//! UI state — separate from business kernel for immediate-mode rendering.

use crate::input::InputBuffer;
use crate::message_list::MessageView;
use crate::notification_center::NotificationCenter;
use crate::overlay::OverlayStack;
use crate::permission_bar::PermissionBar;
use crate::search::SearchState;
use crate::selector::MessageSelector;
use crate::statusline::Statusline;
use crate::statusline_setup::StatuslineSetup;
use crate::themes::Theme;

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

/// Layout mode for the main content area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// Single pane: full width for messages.
    SinglePane,
    /// Horizontal split: left pane shows file/content, right pane shows chat.
    HorizontalSplit {
        /// Width percentage for the left pane (10-90).
        left_percent: u16,
    },
}

impl Default for LayoutMode {
    fn default() -> Self {
        Self::SinglePane
    }
}

/// A single visible plan step in the interactive TUI.
#[derive(Debug, Clone, Default)]
pub struct PlanStep {
    /// Short user-facing label.
    pub label: String,
    /// Whether this step has completed.
    pub complete: bool,
}

/// Lightweight plan state for the current interactive turn.
#[derive(Debug, Clone, Default)]
pub struct InteractivePlan {
    /// Visible steps tracked for the current turn.
    pub steps: Vec<PlanStep>,
    /// Zero-based current step index.
    pub current_step: usize,
    /// Label for the active step.
    pub current_label: Option<String>,
}

impl InteractivePlan {
    /// Clear the current plan.
    pub fn reset(&mut self) {
        self.steps.clear();
        self.current_step = 0;
        self.current_label = None;
    }

    /// Render a short statusline label like "Step 2/4: Running tests".
    #[must_use]
    pub fn status_label(&self) -> Option<String> {
        let total = self.steps.len();
        let label = self.current_label.as_ref()?;
        if total == 0 {
            Some(label.clone())
        } else {
            Some(format!(
                "Step {}/{}: {}",
                self.current_step.saturating_add(1).min(total),
                total,
                label
            ))
        }
    }

    /// Render the full plan for an overlay.
    #[must_use]
    pub fn render_detail(&self) -> String {
        if self.steps.is_empty() {
            return "No active plan.\nStart a turn to populate the current plan.".into();
        }

        let mut lines = Vec::with_capacity(self.steps.len() + 2);
        lines.push(format!(
            "Current step: {}/{}",
            self.current_step.saturating_add(1).min(self.steps.len()),
            self.steps.len()
        ));
        lines.push(String::new());
        for (idx, step) in self.steps.iter().enumerate() {
            let marker = if step.complete {
                "[x]"
            } else if idx == self.current_step {
                "[>]"
            } else {
                "[ ]"
            };
            lines.push(format!("{marker} {}", step.label));
        }
        lines.join("\n")
    }
}

/// Richer permission-request details for overlays and notifications.
#[derive(Debug, Clone, Default)]
pub struct PermissionDetail {
    /// Requested tool/action.
    pub tool: String,
    /// Raw tool arguments summary.
    pub args: String,
    /// Risk summary or explanation shown to the user.
    pub risk_summary: String,
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
    /// Permission bar (below input line).
    pub permission_bar: PermissionBar,
    /// Statusline setup wizard state.
    pub statusline_setup: StatuslineSetup,
    /// Notification center.
    pub notifications: NotificationCenter,
    /// Active semantic color theme.
    pub theme: Theme,
    /// Lightweight plan state for the current interactive turn.
    pub plan: InteractivePlan,
    /// Recent concise system activity rows for overlays and notifications.
    pub activity_log: Vec<String>,
    /// Details for the currently pending permission request.
    pub permission_detail: Option<PermissionDetail>,
    /// Whether compact mode is forced even on large terminals.
    pub force_compact: bool,
    /// Layout mode (single pane or split view).
    pub layout_mode: LayoutMode,
    /// Content shown in the left pane of split view.
    pub left_pane_content: String,
    /// Title shown on the left pane border (e.g., filename).
    pub left_pane_title: String,
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
            permission_bar: PermissionBar::new(),
            statusline_setup: StatuslineSetup::new(),
            notifications: NotificationCenter::new(),
            theme: Theme::dark(),
            plan: InteractivePlan::default(),
            activity_log: Vec::new(),
            permission_detail: None,
            force_compact: false,
            layout_mode: LayoutMode::SinglePane,
            left_pane_content: String::new(),
            left_pane_title: String::new(),
        }
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}
