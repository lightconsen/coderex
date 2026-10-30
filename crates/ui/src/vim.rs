//! Vim mode — normal/insert/visual mode switching with Vim keybindings.
//!
//! When enabled, the input layer switches between Vim's normal and insert modes.
//! Normal mode provides hjkl navigation, dd/yy/p editing, and : command access.
//! Insert mode behaves like a standard text input. Visual mode (`v`) and
//! Visual-line mode (`V`) select part of the input buffer for yanking or
//! deleting; the selection anchor itself lives with the buffer (`InputBuffer`),
//! because only the buffer knows the text being selected.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Vim editing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimMode {
    /// Normal mode — navigation, delete, yank, paste.
    Normal,
    /// Insert mode — free text entry.
    Insert,
    /// Visual mode — characterwise selection.
    Visual,
    /// Visual-line mode — linewise selection.
    VisualLine,
}

/// An action produced by a Vim normal- or Visual-mode key event.
///
/// A variant carrying a `usize` is a counted motion or operator: the number is
/// the count prefix (`3w`, `5x`), and it is always at least `1`. The UI layer
/// repeats the action that many times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VimAction {
    /// Move cursor left, `count` times.
    Left(usize),
    /// Move cursor right, `count` times.
    Right(usize),
    /// Move cursor up `count` lines (e.g., in message list).
    Up(usize),
    /// Move cursor down `count` lines (e.g., in message list).
    Down(usize),
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
    /// Delete `count` whole lines starting at the current one (`dd`).
    DeleteLine(usize),
    /// Delete from the cursor to the start of the next word, `count` times
    /// (`dw`).
    DeleteWordForward(usize),
    /// Delete from the start of the previous word to the cursor, `count` times
    /// (`db`).
    DeleteWordBackward(usize),
    /// Delete character under cursor, `count` times.
    DeleteChar(usize),
    /// Yank (copy) `count` whole lines starting at the current one (`yy`).
    YankLine(usize),
    /// Paste from yank register at cursor, `count` times.
    Paste(usize),
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
    /// Enter characterwise Visual mode (`v`).
    EnterVisual,
    /// Enter linewise Visual mode (`V`).
    EnterVisualLine,
    /// Leave Visual mode without changing the buffer.
    ExitVisual,
    /// Move to the start of the next word, `count` times.
    WordForward(usize),
    /// Move to the start of the previous word, `count` times.
    WordBackward(usize),
    /// Move to the end of the current or next word, `count` times.
    WordEnd(usize),
    /// Yank the Visual selection and leave Visual mode.
    YankSelection,
    /// Delete the Visual selection and leave Visual mode.
    DeleteSelection,
    /// Replace the Visual selection with the yank register and leave Visual
    /// mode (Visual `p`).
    PasteOverSelection,
    /// Delete the Visual selection and enter insert mode (Visual `c`/`s`).
    ChangeSelection,
    /// Open a line below the current one and enter insert mode (`o`).
    OpenLineBelow,
    /// Open a line above the current one and enter insert mode (`O`).
    OpenLineAbove,
    /// Clear `count` whole lines starting at the current one and enter insert
    /// mode (`cc`, `S`).
    ChangeLine(usize),
    /// Change from the cursor to the end of the current word (`cw`).
    ChangeWord,
    /// Change from the cursor to the end of the line (`C`, `c$`).
    ChangeToEnd,
    /// Delete `count` characters under the cursor and enter insert mode (`s`).
    Substitute(usize),
    /// Indent the current line by `count` levels (`>`).
    Indent(usize),
    /// Outdent the current line by `count` levels (`<`).
    Outdent(usize),
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
    /// Whether the register holds whole lines (`yy`, `dd`, linewise Visual)
    /// rather than a character range. A linewise paste inserts new lines
    /// instead of splicing text at the cursor.
    pub yank_linewise: bool,
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
    /// Change operation.
    Change,
}

impl Default for VimState {
    fn default() -> Self {
        Self::new()
    }
}

impl VimState {
    /// Create a new VimState in normal mode.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mode: VimMode::Normal,
            yank_register: String::new(),
            yank_linewise: false,
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
            VimMode::Visual | VimMode::VisualLine => self.handle_visual(event),
        }
    }

    /// Handle keys in insert mode.
    const fn handle_insert(&mut self, event: KeyEvent) -> VimAction {
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

        if self.accumulate_count(event.code) {
            return VimAction::Ignore;
        }
        let count = self.count.unwrap_or(1);

        let action = match event.code {
            // Navigation
            KeyCode::Char('h') | KeyCode::Left => VimAction::Left(count),
            KeyCode::Char('l') | KeyCode::Right => VimAction::Right(count),
            KeyCode::Char('j') | KeyCode::Down => VimAction::Down(count),
            KeyCode::Char('k') | KeyCode::Up => VimAction::Up(count),
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
            KeyCode::Char('i' | 'I') if event.modifiers.contains(KeyModifiers::SHIFT) => {
                VimAction::InsertAtStart
            }
            KeyCode::Char('a' | 'A') if event.modifiers.contains(KeyModifiers::SHIFT) => {
                VimAction::InsertAtEnd
            }
            KeyCode::Char('I') => VimAction::InsertAtStart,
            KeyCode::Char('A') => VimAction::InsertAtEnd,
            KeyCode::Char('i') => VimAction::Insert,
            KeyCode::Char('a') => VimAction::Append,
            KeyCode::Char('o') => VimAction::OpenLineBelow,
            KeyCode::Char('O') => VimAction::OpenLineAbove,

            // Visual mode entry
            KeyCode::Char('v') => {
                self.mode = VimMode::Visual;
                VimAction::EnterVisual
            }
            KeyCode::Char('V') => {
                self.mode = VimMode::VisualLine;
                VimAction::EnterVisualLine
            }

            // Command and search
            KeyCode::Char(':') => VimAction::Command,
            KeyCode::Char('/') => VimAction::Search,

            // Delete operations
            KeyCode::Char('d') => {
                if self.pending_op == Some(VimOperator::Delete) {
                    // dd — delete line
                    self.pending_op = None;
                    self.count = None;
                    VimAction::DeleteLine(count)
                } else {
                    self.pending_op = Some(VimOperator::Delete);
                    VimAction::Ignore
                }
            }
            KeyCode::Char('D') => VimAction::DeleteToEnd,
            KeyCode::Char('x') | KeyCode::Backspace => VimAction::DeleteChar(count),
            KeyCode::Char('X') => {
                self.pending_op = None;
                self.count = None;
                VimAction::DeleteChar(count) // simplified backspace-delete
            }
            // Word motions. `cw` is a change, `dw`/`db` are deletes, and a bare
            // motion is a motion — as in Vim.
            KeyCode::Char('w') => match self.pending_op {
                Some(VimOperator::Change) => VimAction::ChangeWord,
                Some(VimOperator::Delete) => VimAction::DeleteWordForward(count),
                _ => VimAction::WordForward(count),
            },
            KeyCode::Char('b') => match self.pending_op {
                Some(VimOperator::Delete) => VimAction::DeleteWordBackward(count),
                _ => VimAction::WordBackward(count),
            },
            KeyCode::Char('e') => VimAction::WordEnd(count),
            // `s` substitutes characters; `S` changes the whole line.
            KeyCode::Char('s') => VimAction::Substitute(count),
            KeyCode::Char('S') => VimAction::ChangeLine(count),

            // Change operator
            KeyCode::Char('c') => {
                if self.pending_op == Some(VimOperator::Change) {
                    // cc — change the whole line.
                    self.pending_op = None;
                    self.count = None;
                    VimAction::ChangeLine(count)
                } else {
                    self.pending_op = Some(VimOperator::Change);
                    VimAction::Ignore
                }
            }
            KeyCode::Char('C') => VimAction::ChangeToEnd,

            // Indent / outdent (`>>` / `<<` on the current line).
            KeyCode::Char('>') => VimAction::Indent(count),
            KeyCode::Char('<') => VimAction::Outdent(count),

            // Yank
            KeyCode::Char('y') => {
                if self.pending_op == Some(VimOperator::Yank) {
                    self.pending_op = None;
                    self.count = None;
                    VimAction::YankLine(count)
                } else {
                    self.pending_op = Some(VimOperator::Yank);
                    VimAction::Ignore
                }
            }

            // Paste
            KeyCode::Char('p' | 'P') => VimAction::Paste(count),

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

    /// Fold a digit into the count prefix. Returns `true` when the key was a
    /// count digit and was consumed.
    ///
    /// A leading `0` (no count yet) is *not* a count — it is the "start of
    /// line" motion — so it falls through to the caller's match. `0` after a
    /// count (`10`) is a digit.
    fn accumulate_count(&mut self, code: KeyCode) -> bool {
        let KeyCode::Char(c) = code else {
            return false;
        };
        let Some(digit) = c.to_digit(10) else {
            return false;
        };
        if self.count.is_some() || digit != 0 {
            let current = self.count.unwrap_or(0);
            self.count = Some(current * 10 + digit as usize);
            return true;
        }
        false
    }

    /// Handle keys in Visual or Visual-line mode.
    ///
    /// Motions are returned as actions; the UI layer applies them to the input
    /// buffer, which owns the selection anchor and the text. Operators (`y`,
    /// `d`, `x`) leave Visual mode and are applied to the selected range.
    fn handle_visual(&mut self, event: KeyEvent) -> VimAction {
        if event.modifiers.contains(KeyModifiers::CONTROL) {
            // Ctrl+C leaves Visual mode, matching its "get me out" role in
            // insert mode. Other control chords have no selection meaning in a
            // single-line buffer.
            return match event.code {
                KeyCode::Char('c') => {
                    self.leave_visual();
                    VimAction::ExitVisual
                }
                _ => VimAction::Ignore,
            };
        }

        let linewise = self.mode == VimMode::VisualLine;
        if self.accumulate_count(event.code) {
            return VimAction::Ignore;
        }
        let count = self.count.unwrap_or(1);
        let action = match event.code {
            KeyCode::Esc => {
                self.leave_visual();
                VimAction::ExitVisual
            }
            // `v` exits characterwise Visual, and switches linewise Visual
            // back to characterwise. `V` is the mirror image.
            KeyCode::Char('v') => {
                if linewise {
                    self.mode = VimMode::Visual;
                    VimAction::EnterVisual
                } else {
                    self.leave_visual();
                    VimAction::ExitVisual
                }
            }
            KeyCode::Char('V') => {
                if linewise {
                    self.leave_visual();
                    VimAction::ExitVisual
                } else {
                    self.mode = VimMode::VisualLine;
                    VimAction::EnterVisualLine
                }
            }

            // Motions. On a single-line buffer j/k have nowhere to go; the UI
            // layer treats them as no-ops rather than walking history.
            KeyCode::Char('h') | KeyCode::Left => VimAction::Left(count),
            KeyCode::Char('l') | KeyCode::Right => VimAction::Right(count),
            KeyCode::Char('j') | KeyCode::Down => VimAction::Down(count),
            KeyCode::Char('k') | KeyCode::Up => VimAction::Up(count),
            KeyCode::Char('w') => VimAction::WordForward(count),
            KeyCode::Char('b') => VimAction::WordBackward(count),
            KeyCode::Char('e') => VimAction::WordEnd(count),
            KeyCode::Char('0') | KeyCode::Home => VimAction::JumpToStart,
            KeyCode::Char('$') | KeyCode::End => VimAction::JumpToEnd,
            KeyCode::Char('g') => VimAction::JumpTop,
            KeyCode::Char('G') => VimAction::JumpBottom,

            // Operators.
            KeyCode::Char('y') => {
                self.leave_visual();
                VimAction::YankSelection
            }
            KeyCode::Char('d' | 'x') => {
                self.leave_visual();
                VimAction::DeleteSelection
            }
            // `p` replaces the selection with the register.
            KeyCode::Char('p' | 'P') => {
                self.leave_visual();
                VimAction::PasteOverSelection
            }
            // `c`/`s` delete the selection and enter insert mode.
            KeyCode::Char('c' | 's') => {
                self.leave_visual();
                VimAction::ChangeSelection
            }
            // Indenting a selection of a single line is indenting the line.
            KeyCode::Char('>') => {
                self.leave_visual();
                VimAction::Indent(count)
            }
            KeyCode::Char('<') => {
                self.leave_visual();
                VimAction::Outdent(count)
            }

            _ => VimAction::Ignore,
        };

        // A count is consumed by the action it preceded, exactly as in normal
        // mode. Leaving it set would make the next digit extend it: `3w` then
        // `2l` would read as `32l`, not `3w` followed by `2l`.
        if !matches!(action, VimAction::Ignore) {
            self.count = None;
        }
        action
    }

    /// Return to normal mode and drop any pending count/operator state.
    const fn leave_visual(&mut self) {
        self.mode = VimMode::Normal;
        self.count = None;
        self.pending_op = None;
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

    /// Check if currently in either Visual mode.
    #[must_use]
    pub const fn is_visual(&self) -> bool {
        matches!(self.mode, VimMode::Visual | VimMode::VisualLine)
    }

    /// Toggle between normal and insert mode. Visual modes return to normal:
    /// there is no insert equivalent to toggle into.
    pub const fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            VimMode::Normal => VimMode::Insert,
            VimMode::Insert | VimMode::Visual | VimMode::VisualLine => VimMode::Normal,
        };
    }
}

/// Vim mode indicator for the statusline.
#[must_use]
pub const fn mode_indicator(mode: VimMode) -> &'static str {
    match mode {
        VimMode::Normal => "NORMAL",
        VimMode::Insert => "INSERT",
        VimMode::Visual => "VISUAL",
        VimMode::VisualLine => "VISUAL LINE",
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
        assert_eq!(state.handle_key(key('h')), VimAction::Left(1));
        assert_eq!(state.handle_key(key('j')), VimAction::Down(1));
        assert_eq!(state.handle_key(key('k')), VimAction::Up(1));
        assert_eq!(state.handle_key(key('l')), VimAction::Right(1));
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
        assert_eq!(state.handle_key(key('d')), VimAction::DeleteLine(1));
        // Pending cleared
        assert!(state.pending_op.is_none());
    }

    #[test]
    fn yank_line_yy() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('y')), VimAction::Ignore);
        assert_eq!(state.pending_op, Some(VimOperator::Yank));
        assert_eq!(state.handle_key(key('y')), VimAction::YankLine(1));
        assert!(state.pending_op.is_none());
    }

    #[test]
    fn paste_puts_yanked_text() {
        let mut state = VimState::new();
        state.yank("hello world");
        assert_eq!(state.handle_key(key('p')), VimAction::Paste(1));
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
        assert_eq!(state.handle_key(key('x')), VimAction::DeleteChar(1));
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
        assert_eq!(state.handle_key(key('j')), VimAction::Down(3));
        // count cleared after action
        assert!(state.count.is_none());
    }

    /// The count must reach the action, not just sit in `VimState`: a motion
    /// the UI can only apply once would silently drop it.
    #[test]
    fn count_prefix_is_carried_into_the_action() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('3')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('l')), VimAction::Right(3));

        // Multi-digit counts build up.
        assert_eq!(state.handle_key(key('1')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('2')), VimAction::Ignore);
        assert_eq!(state.count, Some(12));
        assert_eq!(state.handle_key(key('x')), VimAction::DeleteChar(12));

        // Counted word motions and paste too.
        assert_eq!(state.handle_key(key('2')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('w')), VimAction::WordForward(2));
        assert_eq!(state.handle_key(key('4')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('p')), VimAction::Paste(4));
    }

    /// A leading `0` is the start-of-line motion, not a count of zero. `10` is
    /// still a count because the `0` follows a digit.
    #[test]
    fn leading_zero_is_a_motion_not_a_count() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('0')), VimAction::JumpToStart);

        assert_eq!(state.handle_key(key('1')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('0')), VimAction::Ignore);
        assert_eq!(state.count, Some(10));
        assert_eq!(state.handle_key(key('l')), VimAction::Right(10));
    }

    #[test]
    fn open_line_below_and_above() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('o')), VimAction::OpenLineBelow);
        assert_eq!(state.handle_key(key('O')), VimAction::OpenLineAbove);
    }

    #[test]
    fn change_operator_cc_cw_and_change_to_end() {
        let mut state = VimState::new();
        // `cc` changes the line.
        assert_eq!(state.handle_key(key('c')), VimAction::Ignore);
        assert_eq!(state.pending_op, Some(VimOperator::Change));
        assert_eq!(state.handle_key(key('c')), VimAction::ChangeLine(1));
        assert!(state.pending_op.is_none());

        // `cw` changes the word.
        assert_eq!(state.handle_key(key('c')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('w')), VimAction::ChangeWord);

        // `C` changes to the end of the line directly.
        assert_eq!(state.handle_key(key('C')), VimAction::ChangeToEnd);
    }

    #[test]
    fn substitute_s_and_change_line() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('s')), VimAction::Substitute(1));
        assert_eq!(state.handle_key(key('3')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('s')), VimAction::Substitute(3));
        assert_eq!(state.handle_key(key('S')), VimAction::ChangeLine(1));
    }

    #[test]
    fn indent_and_outdent_carry_the_count() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('>')), VimAction::Indent(1));
        assert_eq!(state.handle_key(key('<')), VimAction::Outdent(1));
        assert_eq!(state.handle_key(key('2')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('>')), VimAction::Indent(2));
    }

    #[test]
    fn visual_paste_over_and_change() {
        let mut paste = visual_state();
        assert_eq!(paste.handle_key(key('p')), VimAction::PasteOverSelection);
        assert_eq!(paste.mode, VimMode::Normal);

        let mut change = visual_state();
        assert_eq!(change.handle_key(key('c')), VimAction::ChangeSelection);
        assert_eq!(change.mode, VimMode::Normal);

        let mut substitute = visual_state();
        assert_eq!(substitute.handle_key(key('s')), VimAction::ChangeSelection);
        assert_eq!(substitute.mode, VimMode::Normal);
    }

    #[test]
    fn visual_indent_leaves_visual_mode() {
        let mut state = visual_state();
        assert_eq!(state.handle_key(key('>')), VimAction::Indent(1));
        assert_eq!(state.mode, VimMode::Normal);

        let mut state = visual_state();
        assert_eq!(state.handle_key(key('<')), VimAction::Outdent(1));
        assert_eq!(state.mode, VimMode::Normal);
    }

    #[test]
    fn visual_motions_carry_the_count() {
        let mut state = visual_state();
        assert_eq!(state.handle_key(key('3')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('w')), VimAction::WordForward(3));
        assert_eq!(state.handle_key(key('2')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('l')), VimAction::Right(2));
        assert!(state.is_visual());
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
            VimAction::Left(1)
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            VimAction::Right(1)
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            VimAction::Up(1)
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            VimAction::Down(1)
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

    fn visual_state() -> VimState {
        let mut state = VimState::new();
        state.mode = VimMode::Visual;
        state
    }

    #[test]
    fn v_enters_visual_and_escape_leaves() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('v')), VimAction::EnterVisual);
        assert_eq!(state.mode, VimMode::Visual);
        assert!(state.is_visual());

        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(state.handle_key(esc), VimAction::ExitVisual);
        assert_eq!(state.mode, VimMode::Normal);
        assert!(!state.is_visual());
    }

    #[test]
    fn shift_v_enters_visual_line() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(shift_key('V')), VimAction::EnterVisualLine);
        assert_eq!(state.mode, VimMode::VisualLine);

        // V again leaves linewise visual mode.
        assert_eq!(state.handle_key(shift_key('V')), VimAction::ExitVisual);
        assert_eq!(state.mode, VimMode::Normal);
    }

    #[test]
    fn visual_v_toggles_linewise_and_exits() {
        let mut state = VimState::new();
        state.mode = VimMode::VisualLine;

        assert_eq!(state.handle_key(key('v')), VimAction::EnterVisual);
        assert_eq!(state.mode, VimMode::Visual);

        assert_eq!(state.handle_key(key('v')), VimAction::ExitVisual);
        assert_eq!(state.mode, VimMode::Normal);
    }

    #[test]
    fn visual_motions() {
        let mut state = visual_state();
        assert_eq!(state.handle_key(key('h')), VimAction::Left(1));
        assert_eq!(state.handle_key(key('l')), VimAction::Right(1));
        assert_eq!(state.handle_key(key('j')), VimAction::Down(1));
        assert_eq!(state.handle_key(key('k')), VimAction::Up(1));
        assert_eq!(state.handle_key(key('w')), VimAction::WordForward(1));
        assert_eq!(state.handle_key(key('b')), VimAction::WordBackward(1));
        assert_eq!(state.handle_key(key('e')), VimAction::WordEnd(1));
        assert_eq!(state.handle_key(key('0')), VimAction::JumpToStart);
        assert_eq!(state.handle_key(key('$')), VimAction::JumpToEnd);
        assert_eq!(state.handle_key(key('g')), VimAction::JumpTop);
        assert_eq!(state.handle_key(key('G')), VimAction::JumpBottom);
        // Motions do not leave visual mode.
        assert!(state.is_visual());
    }

    /// `w` means "delete word" in this codebase's normal mode, but it must be
    /// the word motion in Visual mode.
    #[test]
    fn visual_w_is_a_motion_not_delete_word() {
        let mut normal = VimState::new();
        assert_eq!(normal.handle_key(key('w')), VimAction::WordForward(1));

        let mut visual = visual_state();
        assert_eq!(visual.handle_key(key('w')), VimAction::WordForward(1));
    }

    #[test]
    fn y_yanks_visual_selection_and_leaves_visual() {
        let mut state = visual_state();
        assert_eq!(state.handle_key(key('y')), VimAction::YankSelection);
        assert_eq!(state.mode, VimMode::Normal);
    }

    #[test]
    fn d_and_x_delete_visual_selection() {
        let mut state = visual_state();
        assert_eq!(state.handle_key(key('d')), VimAction::DeleteSelection);
        assert_eq!(state.mode, VimMode::Normal);

        let mut state = visual_state();
        assert_eq!(state.handle_key(key('x')), VimAction::DeleteSelection);
        assert_eq!(state.mode, VimMode::Normal);
    }

    #[test]
    fn ctrl_c_leaves_visual() {
        let mut state = visual_state();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(state.handle_key(ctrl_c), VimAction::ExitVisual);
        assert_eq!(state.mode, VimMode::Normal);
    }

    #[test]
    fn visual_mode_does_not_change_insert_or_normal_bindings() {
        let mut insert = VimState::new_insert();
        assert_eq!(insert.handle_key(key('v')), VimAction::Insert);

        let mut normal = VimState::new();
        assert_eq!(normal.handle_key(key('x')), VimAction::DeleteChar(1));
        assert_eq!(normal.handle_key(key('y')), VimAction::Ignore);
        assert_eq!(normal.handle_key(key('y')), VimAction::YankLine(1));
    }

    #[test]
    fn visual_mode_indicator_strings() {
        assert_eq!(super::mode_indicator(VimMode::Visual), "VISUAL");
        assert_eq!(super::mode_indicator(VimMode::VisualLine), "VISUAL LINE");
    }

    /// `w`, `b` and `e` are motions in normal mode, not edits. `w` used to
    /// delete the word before the cursor, which made it impossible to move
    /// through the buffer without changing it.
    #[test]
    fn normal_word_motions_are_motions() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('w')), VimAction::WordForward(1));
        assert_eq!(state.handle_key(key('b')), VimAction::WordBackward(1));
        assert_eq!(state.handle_key(key('e')), VimAction::WordEnd(1));

        assert_eq!(state.handle_key(key('2')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('w')), VimAction::WordForward(2));
        assert_eq!(state.handle_key(key('3')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('b')), VimAction::WordBackward(3));
        assert_eq!(state.handle_key(key('4')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('e')), VimAction::WordEnd(4));
    }

    /// The operator still applies to the word motions it can complete: `dw`
    /// deletes forwards, `db` backwards, `cw` changes the word.
    #[test]
    fn delete_and_change_operators_take_word_motions() {
        let mut delete_forward = VimState::new();
        assert_eq!(delete_forward.handle_key(key('d')), VimAction::Ignore);
        assert_eq!(
            delete_forward.handle_key(key('w')),
            VimAction::DeleteWordForward(1)
        );
        assert_eq!(delete_forward.handle_key(key('2')), VimAction::Ignore);
        assert_eq!(delete_forward.handle_key(key('d')), VimAction::Ignore);
        assert_eq!(
            delete_forward.handle_key(key('w')),
            VimAction::DeleteWordForward(2)
        );

        let mut delete_backward = VimState::new();
        assert_eq!(delete_backward.handle_key(key('d')), VimAction::Ignore);
        assert_eq!(
            delete_backward.handle_key(key('b')),
            VimAction::DeleteWordBackward(1)
        );

        let mut change = VimState::new();
        assert_eq!(change.handle_key(key('c')), VimAction::Ignore);
        assert_eq!(change.handle_key(key('w')), VimAction::ChangeWord);
    }

    /// `3dd`, `3yy` and `3cc` carry the count into the action so the buffer can
    /// remove or copy three lines.
    #[test]
    fn line_operators_carry_the_count() {
        let mut state = VimState::new();
        assert_eq!(state.handle_key(key('3')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('d')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('d')), VimAction::DeleteLine(3));

        assert_eq!(state.handle_key(key('2')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('y')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('y')), VimAction::YankLine(2));

        assert_eq!(state.handle_key(key('4')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('c')), VimAction::Ignore);
        assert_eq!(state.handle_key(key('c')), VimAction::ChangeLine(4));

        // A bare `S` changes exactly the one line.
        assert_eq!(state.handle_key(key('S')), VimAction::ChangeLine(1));
    }

    /// A linewise register is recorded so `p` can insert whole lines rather
    /// than splice text at the cursor.
    #[test]
    fn yank_linewise_flag_defaults_off_and_is_settable() {
        let mut state = VimState::new();
        assert!(!state.yank_linewise);
        state.yank("one\ntwo\n");
        state.yank_linewise = true;
        assert_eq!(state.yanked(), "one\ntwo\n");
        assert!(state.yank_linewise);
    }
}
