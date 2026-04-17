//! Statusline Setup — guided wizard for configuring statusline preferences.
//!
//! Walks the user through steps to configure which statusline segments to show
//! (model, tokens, cost, git branch) and applies the preferences.

use crate::statusline::Statusline;

/// A step in the setup wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStep {
    /// Welcome/introduction step.
    Welcome,
    /// Choose preferred model.
    ModelSelect,
    /// Toggle token count display.
    TokenTracking,
    /// Toggle cost display.
    CostDisplay,
    /// Toggle git branch display.
    GitBranch,
    /// Setup complete.
    Complete,
}

impl SetupStep {
    /// Next step in the wizard.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Welcome => Self::ModelSelect,
            Self::ModelSelect => Self::TokenTracking,
            Self::TokenTracking => Self::CostDisplay,
            Self::CostDisplay => Self::GitBranch,
            Self::GitBranch => Self::Complete,
            Self::Complete => Self::Complete,
        }
    }

    /// Previous step in the wizard.
    #[must_use]
    pub fn prev(self) -> Self {
        match self {
            Self::Welcome => Self::Welcome,
            Self::ModelSelect => Self::Welcome,
            Self::TokenTracking => Self::ModelSelect,
            Self::CostDisplay => Self::TokenTracking,
            Self::GitBranch => Self::CostDisplay,
            Self::Complete => Self::GitBranch,
        }
    }

    /// Prompt text for the current step.
    #[must_use]
    pub fn prompt(&self) -> &'static str {
        match self {
            Self::Welcome => "Welcome! Configure your statusline.",
            Self::ModelSelect => "Preferred model: sonnet/opus/haiku?",
            Self::TokenTracking => "Show token count? [y/n]",
            Self::CostDisplay => "Show cost? [y/n]",
            Self::GitBranch => "Show git branch? [y/n]",
            Self::Complete => "Setup complete! Press Enter to apply.",
        }
    }
}

/// Statusline setup wizard state.
#[derive(Debug, Clone)]
pub struct StatuslineSetup {
    /// Current step in the wizard.
    pub current_step: SetupStep,
    /// Preferred model name.
    pub model_preference: String,
    /// Whether to show token count.
    pub show_tokens: bool,
    /// Whether to show cost.
    pub show_cost: bool,
    /// Whether to show git branch.
    pub show_git_branch: bool,
    /// Whether setup is complete.
    pub completed: bool,
}

impl Default for StatuslineSetup {
    fn default() -> Self {
        Self::new()
    }
}

impl StatuslineSetup {
    /// Create a new setup wizard.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current_step: SetupStep::Welcome,
            model_preference: "sonnet".into(),
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
            completed: false,
        }
    }

    /// Advance to the next step.
    pub fn next_step(&mut self) {
        if self.current_step != SetupStep::Complete {
            self.current_step = self.current_step.next();
            if self.current_step == SetupStep::Complete {
                self.completed = true;
            }
        }
    }

    /// Go back to the previous step.
    pub fn back_step(&mut self) {
        self.current_step = self.current_step.prev();
    }

    /// Toggle the option for the current step.
    pub fn toggle_current(&mut self) {
        match self.current_step {
            SetupStep::TokenTracking => self.show_tokens = !self.show_tokens,
            SetupStep::CostDisplay => self.show_cost = !self.show_cost,
            SetupStep::GitBranch => self.show_git_branch = !self.show_git_branch,
            _ => {}
        }
    }

    /// Apply configured preferences to a statusline.
    pub fn apply_to_statusline(&self, sl: &mut Statusline) {
        sl.show_tokens = self.show_tokens;
        sl.show_cost = self.show_cost;
        sl.show_git_branch = self.show_git_branch;
        if !self.model_preference.is_empty() {
            sl.model.clone_from(&self.model_preference);
        }
    }

    /// Check if setup is complete.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.completed
    }

    /// Get the prompt text for the current step.
    #[must_use]
    pub fn render_prompt(&self) -> &str {
        self.current_step.prompt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_advance_in_order() {
        let mut setup = StatuslineSetup::new();
        assert_eq!(setup.current_step, SetupStep::Welcome);
        setup.next_step();
        assert_eq!(setup.current_step, SetupStep::ModelSelect);
        setup.next_step();
        assert_eq!(setup.current_step, SetupStep::TokenTracking);
        setup.next_step();
        assert_eq!(setup.current_step, SetupStep::CostDisplay);
        setup.next_step();
        assert_eq!(setup.current_step, SetupStep::GitBranch);
        setup.next_step();
        assert_eq!(setup.current_step, SetupStep::Complete);
    }

    #[test]
    fn back_navigation_works() {
        let mut setup = StatuslineSetup::new();
        setup.next_step(); // ModelSelect
        setup.next_step(); // TokenTracking
        assert_eq!(setup.current_step, SetupStep::TokenTracking);
        setup.back_step();
        assert_eq!(setup.current_step, SetupStep::ModelSelect);
        setup.back_step();
        assert_eq!(setup.current_step, SetupStep::Welcome);
        setup.back_step(); // Can't go before Welcome
        assert_eq!(setup.current_step, SetupStep::Welcome);
    }

    #[test]
    fn toggle_options_changes_preferences() {
        let mut setup = StatuslineSetup::new();
        setup.current_step = SetupStep::TokenTracking;
        assert!(setup.show_tokens);
        setup.toggle_current();
        assert!(!setup.show_tokens);
        setup.toggle_current();
        assert!(setup.show_tokens);

        setup.current_step = SetupStep::CostDisplay;
        setup.toggle_current();
        assert!(!setup.show_cost);

        setup.current_step = SetupStep::GitBranch;
        setup.toggle_current();
        assert!(!setup.show_git_branch);
    }

    #[test]
    fn apply_updates_statusline() {
        let mut setup = StatuslineSetup::new();
        setup.model_preference = "opus".into();
        setup.show_tokens = false;
        setup.show_cost = false;

        let mut sl = Statusline::new();
        setup.apply_to_statusline(&mut sl);
        assert_eq!(sl.model, "opus");
        assert!(!sl.show_tokens);
        assert!(!sl.show_cost);
    }

    #[test]
    fn complete_flag_set_after_final_step() {
        let mut setup = StatuslineSetup::new();
        assert!(!setup.completed);
        for _ in 0..5 {
            setup.next_step();
        }
        assert!(setup.completed);
        assert_eq!(setup.current_step, SetupStep::Complete);
    }

    #[test]
    fn render_prompt_shows_current_step_text() {
        let mut setup = StatuslineSetup::new();
        assert!(setup.render_prompt().contains("Welcome"));
        setup.next_step();
        assert!(setup.render_prompt().contains("model"));
    }
}
