//! Vim mode — normal/insert mode switching with Vim keybindings.
//!
//! When enabled, the input layer switches between Vim's normal and insert modes.
//! Normal mode provides hjkl navigation, dd/yy/p editing, and : command access.
//! Insert mode behaves like a standard text input.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Vim editing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    /// Normal mode — navigation, delete, yank, paste.
    Normal,
    /// Insert mode — free text entry.
    Insert,
}

/// An action produced by a Vim normal-mode key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VimAction {
    /// Move cursor left.
    Left,
    /// Move cursor right.
    Right,
    /// Move cursor up (e.g., in message list).
    Up,
    /// Move cursor down (e.g., in message list).
    Down,
    /// Jump to beginning of line.
    JumpToStart,
    /// Jump to end of line.
    JumpToEnd,
    /// Jump to top of viewport.
    JumpTop,
    /// Jump to bottom of viewport.
    JumpBottom,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Half page up (Ctrl+u).
    HalfPageUp,
    /// Half page down (Ctrl+d).
    HalfPageDown,
    /// Delete the current line (content under cursor to end).
    DeleteToEnd,
    /// Delete entire line (input buffer).
    DeleteLine,
    /// Delete word from cursor.
    DeleteWord,
    /// Delete character under cursor.
    DeleteChar,
    /// Yank (copy) the entire line content.
    YankLine,
    /// Paste from yank register at cursor.
    Paste,
    /// Enter insert mode (before cursor).
    Insert,
    /// Enter insert mode at end of line.
    InsertAtEnd,
    /// Enter insert mode at beginning of line.
    InsertAtStart,
    /// Append after cursor.
    Append,
    /// Enter command mode (starts with ":").
    Command,
    /// Search mode (starts with "/").
    Search,
    /// Undo.
    Undo,
    /// Switch to insert mode and submit (like pressing Enter in insert).
    Submit,
    /// Switch back to normal mode.
    Escape,
    /// Repeat last change.
    Repeat,
    /// Toggle fold on focused message.
    ToggleFold,
    /// Next search match.
    SearchNext,
    /// Previous search match.
    SearchPrev,
    /// Export/copy message to clipboard.
    ExportCopy,
    /// Toggle split-pane view.
    ToggleSplit,
    /// Ignore this key.
    Ignore,
}

/// Vim state tracks the editing mode, registers, and history.
#[derive(Debug)]
pub struct VimState {
    /// Current mode.
    pub mode: VimMode,
    /// Yank register (single for now).
    pub yank_register: String,
    /// Number prefix (for repeated actions like 3j).
    pub count: Option<usize>,
    /// Pending operator (e.g., 'd' waiting for motion).
    pub pending_op: Option<VimOperator>,
    /// Undo history for insert-mode changes.
    pub undo_stack: Vec<String>,
    /// Last change to repeat.
    pub last_change: Option<String>,
}

/// A pending Vim operator waiting for a motion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VimOperator {
    /// Delete operation.
    Delete,
    /// Yank (copy) operation.
    Yank,
}

impl Default for VimState {
    fn default() -> Self {
        Self::new()
    }
}

impl VimState {
    /// Create a new VimState in normal mode.
    #[must_use]
    pub fn new() -> Self {
        Self {
            mode: VimMode::Normal,
            yank_register: String::new(),
            count: None,
            pending_op: None,
            undo_stack: Vec::new(),
            last_change: None,
        }
    }

    /// Create a new VimState starting in insert mode.
    #[must_use]
    pub fn new_insert() -> Self {
        Self {
            mode: VimMode::Insert,
            ..Self::new()
        }
    }

    /// Process a key event in the context of the current Vim mode.
    ///
    /// Returns `VimAction` to be applied by the UI layer.
    pub fn handle_key(&mut self, event: KeyEvent) -> VimAction {
        match self.mode {
            VimMode::Insert => self.handle_insert(event),
            VimMode::Normal => self.handle_normal(event),
        }
    }

    /// Handle keys in insert mode.
    fn handle_insert(&mut self, event: KeyEvent) -> VimAction {
        match event.code {
            KeyCode::Esc => {
                self.mode = VimMode::Normal;
                // Move cursor back one (Vim convention: cursor can't go past last char)
                VimAction::Escape
            }
            KeyCode::Enter => VimAction::Submit,
            KeyCode::Char('c') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mode = VimMode::Normal;
                VimAction::Escape
            }
            _ => VimAction::Insert,
        }
    }

    /// Handle keys in normal mode.
    fn handle_normal(&mut self, event: KeyEvent) -> VimAction {
        // Ctrl bindings (half-page scrolling)
        if event.modifiers.contains(KeyModifiers::CONTROL) {
            return match event.code {
                KeyCode::Char('u') => VimAction::HalfPageUp,
                KeyCode::Char('d') => VimAction::HalfPageDown,
                KeyCode::Char('c') => {
                    self.mode = VimMode::Normal;
                    VimAction::Escape
                }
                _ => VimAction::Ignore,
            };
        }

        // Check if we're building a count prefix
        if let KeyCode::Char(c) = event.code {
            if let Some(digit) = c.to_digit(10) {
                // Digits 1-9 start/extend count prefix
                if self.pending_op.is_none() || digit != 0 {
                    let current = self.count.unwrap_or(0);
                    self.count = Some(current * 10 + digit as usize);
                    return VimAction::Ignore;
                }
            }
        }

        let action = match event.code {
            // Navigation
            KeyCode::Char('h') | KeyCode::Left => VimAction::Left,
            KeyCode::Char('l') | KeyCode::Right => VimAction::Right,
            KeyCode::Char('j') | KeyCode::Down => VimAction::Down,
            KeyCode::Char('k') | KeyCode::Up => VimAction::Up,
            KeyCode::Char('0') | KeyCode::Home => VimAction::JumpToStart,
            KeyCode::Char('$') | KeyCode::End => VimAction::JumpToEnd,
            KeyCode::Char('g') if event.modifiers.is_empty() => {
                // Could be 'gg' — for now, handle single g as top
                VimAction::JumpTop
            }
            KeyCode::Char('G') => VimAction::JumpBottom,

            // Scrolling
            KeyCode::PageUp => VimAction::PageUp,
            KeyCode::PageDown => VimAction::PageDown,

            // Insert mode entry
            KeyCode::Char('i') | KeyCode::Char('I')
                if event.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                VimAction::InsertAtStart
            }
            KeyCode::Char('a') | KeyCode::Char('A')
                if event.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                VimAction::InsertAtEnd
            }
            KeyCode::Char('I') => VimAction::InsertAtStart,
            KeyCode::Char('A') => VimAction::InsertAtEnd,
            KeyCode::Char('i') => VimAction::Insert,
            KeyCode::Char('a') => VimAction::Append,

            // Command and search
            KeyCode::Char(':') => VimAction::Command,
            KeyCode::Char('/') => VimAction::Search,

            // Delete operations
            KeyCode::Char('d') => {
                if self.pending_op == Some(VimOperator::Delete) {
                    // dd — delete line
                    self.pending_op = None;
                    self.count = None;
                    VimAction::DeleteLine
                } else {
                    self.pending_op = Some(VimOperator::Delete);
                    VimAction::Ignore
                }
            }
            KeyCode::Char('D') => VimAction::DeleteToEnd,
            KeyCode::Char('x') => VimAction::DeleteChar,
            KeyCode::Char('X') => {
                self.pending_op = None;
                self.count = None;
                VimAction::DeleteChar // simplified backspace-delete
            }
            KeyCode::Char('w') => VimAction::DeleteWord,

            // Yank
            KeyCode::Char('y') => {
                if self.pending_op == Some(VimOperator::Yank) {
                    self.pending_op = None;
                    self.count = None;
                    VimAction::YankLine
                } else {
                    self.pending_op = Some(VimOperator::Yank);
                    VimAction::Ignore
                }
            }

            // Paste
            KeyCode::Char('p') => VimAction::Paste,
            KeyCode::Char('P') => VimAction::Paste, // simplified

            // Undo
            KeyCode::Char('u') => VimAction::Undo,

            // Repeat
            KeyCode::Char('.') => VimAction::Repeat,

            // Fold toggle
            KeyCode::Char('z') => VimAction::ToggleFold,

            // Search navigation
            KeyCode::Char('n') => VimAction::SearchNext,
            KeyCode::Char('N') => VimAction::SearchPrev,

            // Escape (already in normal mode, acts like clear pending)
            KeyCode::Esc => {
                self.pending_op = None;
                self.count = None;
                VimAction::Escape
            }

            // Enter in normal mode = submit
            KeyCode::Enter => VimAction::Submit,

            // Backspace in normal mode = delete char before
            KeyCode::Backspace => VimAction::DeleteChar,

            _ => {
                self.pending_op = None;
                self.count = None;
                VimAction::Ignore
            }
        };

        // Clear pending op and count after a non-ignored action
        if !matches!(action, VimAction::Ignore) {
            self.pending_op = None;
            self.count = None;
        }

        action
    }

    /// Save current text to yank register.
    pub fn yank(&mut self, text: &str) {
        self.yank_register = text.to_string();
    }

    /// Get the yank register content.
    #[must_use]
    pub fn yanked(&self) -> &str {
        &self.yank_register
    }

    /// Push to undo stack.
    pub fn push_undo(&mut self, text: &str) {
        self.undo_stack.push(text.to_string());
        // Limit undo stack size
        if self.undo_stack.len() > 100 {
            self.undo_stack.drain(..50);
        }
    }

    /// Pop from undo stack.
    pub fn pop_undo(&mut self) -> Option<String> {
        self.undo_stack.pop()
    }

    /// Check if currently in normal mode.
    #[must_use]
    pub fn is_normal(&self) -> bool {
        self.mode == VimMode::Normal
    }

    /// Check if currently in insert mode.
    #[must_use]
    pub fn is_insert(&self) -> bool {
        self.mode == VimMode::Insert
    }

    /// Toggle between normal and insert mode.
    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            VimMode::Normal => VimMode::Insert,
            VimMode::Insert => VimMode::Normal,
        };
    }
}

/// Vim mode indicator for the statusline.
#[must_use]
pub fn mode_indicator(mode: VimMode) -> &'static str {
    match mode {
        VimMode::Normal => "NORMAL",
        VimMode::Insert => "INSERT",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn shift_key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
    }

    #[test]
    fn starts_in_normal_mode() {
        let state = VimState::new();
        assert!(state.is_normal());
    }

    #[test]
    fn h_j_k_l_navigation() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('h')), VimAction::Left);
        assert_eq!(state.handle_key(key('j')), VimAction::Down);
        assert_eq!(state.handle_key(key('k')), VimAction::Up);
        assert_eq!(state.handle_key(key('l')), VimAction::Right);
    }

    #[test]
    fn insert_mode_entry_and_exit() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('i')), VimAction::Insert);
        // Insert action doesn't change mode — the UI layer does
        // Simulate the UI switching to insert mode
        state.mode = VimMode::Insert;
        assert!(state.is_insert());

        // Esc exits insert mode
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let action = state.handle_key(esc);
        assert_eq!(action, VimAction::Escape);
        assert!(state.is_normal());
    }

    #[test]
    fn ctrl_c_exits_insert() {
        let mut state = VimState::new_insert();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let action = state.handle_key(ctrl_c);
        assert_eq!(action, VimAction::Escape);
        assert!(state.is_normal());
    }

    #[test]
    fn delete_line_dd() {
        let mut state = VimState::new();
        // First d — sets pending
        assert_eq!(state.handle_key(key('d')), VimAction::Ignore);
        assert_eq!(state.pending_op, Some(VimOperator::Delete));
        // Second d — executes delete line
        assert_eq!(state.handle_key(key('d')), VimAction::DeleteLine);
        // Pending cleared
        assert!(state.pending_op.is_none());
    }

    #[test]
    fn yank_line_yy() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('y')), VimAction::Ignore);
        assert_eq!(state.pending_op, Some(VimOperator::Yank));
        assert_eq!(state.handle_key(key('y')), VimAction::YankLine);
        assert!(state.pending_op.is_none());
    }

    #[test]
    fn paste_puts_yanked_text() {
        let mut state = VimState::new();
        state.yank("hello world");
        assert_eq!(state.handle_key(key('p')), VimAction::Paste);
        assert_eq!(state.yanked(), "hello world");
    }

    #[test]
    fn a_appends_after_cursor() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('a')), VimAction::Append);
    }

    #[test]
    fn insert_at_end() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(shift_key('a')), VimAction::InsertAtEnd);
    }

    #[test]
    fn insert_at_start() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(shift_key('i')), VimAction::InsertAtStart);
    }

    #[test]
    fn colon_enters_command() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key(':')), VimAction::Command);
    }

    #[test]
    fn slash_enters_search() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('/')), VimAction::Search);
    }

    #[test]
    fn x_deletes_char() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('x')), VimAction::DeleteChar);
    }

    #[test]
    fn u_undoes() {
        let mut state = VimState::new();
        state.push_undo("before change");
        state.push_undo("after change");
        assert_eq!(state.handle_key(key('u')), VimAction::Undo);
        // handle_key returns the action; UI layer pops the undo
        assert_eq!(state.pop_undo(), Some("after change".into()));
        assert_eq!(state.pop_undo(), Some("before change".into()));
    }

    #[test]
    fn undo_stack_is_limited() {
        let mut state = VimState::new();
        for i in 0..200 {
            state.push_undo(&format!("state {i}"));
        }
        assert!(state.undo_stack.len() <= 100);
    }

    #[test]
    fn count_prefix_accumulates() {
        let mut state = VimState::new();
        // 3j would be: '3' accumulates count, 'j' returns down with count=3
        assert_eq!(state.handle_key(key('3')), VimAction::Ignore);
        assert_eq!(state.count, Some(3));
        assert_eq!(state.handle_key(key('j')), VimAction::Down);
        // count cleared after action
        assert!(state.count.is_none());
    }

    #[test]
    fn escape_clears_pending_op() {
        let mut state = VimState::new();
        state.handle_key(key('d')); // pending delete
        assert!(state.pending_op.is_some());

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        state.handle_key(esc);
        assert!(state.pending_op.is_none());
        assert!(state.count.is_none());
    }

    #[test]
    fn toggle_mode() {
        let mut state = VimState::new();
        assert!(state.is_normal());
        state.toggle_mode();
        assert!(state.is_insert());
        state.toggle_mode();
        assert!(state.is_normal());
    }

    #[test]
    fn mode_indicator_strings() {
        assert_eq!(super::mode_indicator(VimMode::Normal), "NORMAL");
        assert_eq!(super::mode_indicator(VimMode::Insert), "INSERT");
    }

    #[test]
    fn submit_in_insert() {
        let mut state = VimState::new_insert();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(state.handle_key(enter), VimAction::Submit);
    }

    #[test]
    fn arrow_keys_work_in_normal() {
        let mut state = VimState::new();
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            VimAction::Left
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            VimAction::Right
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            VimAction::Up
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            VimAction::Down
        );
    }

    #[test]
    fn page_up_down_in_normal() {
        let mut state = VimState::new();
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            VimAction::PageUp
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
            VimAction::PageDown
        );
    }

    #[test]
    fn insert_at_end_and_start() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('I')), VimAction::InsertAtStart);
        assert_eq!(state.handle_key(key('A')), VimAction::InsertAtEnd);
    }
}
