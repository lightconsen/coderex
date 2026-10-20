//! Message selector — multi-select messages for copy/export.

/// A message selector that allows multi-selecting messages.
#[derive(Debug)]
pub struct MessageSelector {
    /// Total number of messages.
    total: usize,
    /// Set of selected message indices.
    selected: Vec<usize>,
    /// Currently focused index.
    focus: usize,
}

impl MessageSelector {
    /// Create a new message selector.
    #[must_use]
    pub const fn new(total: usize) -> Self {
        Self {
            total,
            selected: Vec::new(),
            focus: total.saturating_sub(1), // start at last message
        }
    }

    /// Move focus up.
    pub const fn focus_up(&mut self) {
        if self.focus > 0 {
            self.focus -= 1;
        }
    }

    /// Move focus down.
    pub const fn focus_down(&mut self) {
        if self.focus + 1 < self.total {
            self.focus += 1;
        }
    }

    /// Toggle selection at the focused index.
    pub fn toggle_focus(&mut self) {
        if let Some(pos) = self.selected.iter().position(|&i| i == self.focus) {
            self.selected.remove(pos);
        } else {
            self.selected.push(self.focus);
        }
        self.selected.sort_unstable();
    }

    /// Select all messages.
    pub fn select_all(&mut self) {
        self.selected = (0..self.total).collect();
    }

    /// Clear all selections.
    pub fn clear(&mut self) {
        self.selected.clear();
    }

    /// Get the selected indices.
    #[must_use]
    pub fn selected_indices(&self) -> &[usize] {
        &self.selected
    }

    /// Number of selected messages.
    #[must_use]
    pub fn count(&self) -> usize {
        self.selected.len()
    }

    /// Check if the focused index is selected.
    #[must_use]
    pub fn is_focus_selected(&self) -> bool {
        self.selected.contains(&self.focus)
    }

    /// Get the current focus index.
    #[must_use]
    pub const fn focus_index(&self) -> usize {
        self.focus
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_select_deselect() {
        let mut sel = MessageSelector::new(5);
        sel.toggle_focus();
        assert_eq!(sel.count(), 1);
        assert!(sel.is_focus_selected());
        sel.toggle_focus();
        assert_eq!(sel.count(), 0);
        assert!(!sel.is_focus_selected());
    }

    #[test]
    fn focus_navigation() {
        let mut sel = MessageSelector::new(5);
        assert_eq!(sel.focus_index(), 4); // starts at last
        sel.focus_up();
        assert_eq!(sel.focus_index(), 3);
        sel.focus_down();
        assert_eq!(sel.focus_index(), 4);
        sel.focus_down(); // no-op at bottom
        assert_eq!(sel.focus_index(), 4);
        sel.focus_up();
        sel.focus_up();
        sel.focus_up();
        sel.focus_up();
        assert_eq!(sel.focus_index(), 0);
        sel.focus_up(); // no-op at top
        assert_eq!(sel.focus_index(), 0);
    }

    #[test]
    fn select_all() {
        let mut sel = MessageSelector::new(3);
        sel.select_all();
        assert_eq!(sel.count(), 3);
        assert_eq!(sel.selected_indices(), &[0, 1, 2]);
    }

    #[test]
    fn clear_selection() {
        let mut sel = MessageSelector::new(5);
        sel.toggle_focus();
        sel.focus_up();
        sel.toggle_focus();
        assert_eq!(sel.count(), 2);
        sel.clear();
        assert_eq!(sel.count(), 0);
    }

    #[test]
    fn selected_indices_ordered() {
        let mut sel = MessageSelector::new(10);
        sel.focus = 5;
        sel.toggle_focus();
        sel.focus = 2;
        sel.toggle_focus();
        sel.focus = 8;
        sel.toggle_focus();
        assert_eq!(sel.selected_indices(), &[2, 5, 8]);
    }
}
