//! Input handling — keyboard event processing and input buffer editing.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

/// A user input action derived from a key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    /// A character was inserted into the buffer.
    InsertChar(char),
    /// Backspace: delete character before cursor.
    Backspace,
    /// Delete: delete character at cursor.
    Delete,
    /// Move cursor left.
    CursorLeft,
    /// Move cursor right.
    CursorRight,
    /// Move cursor to beginning of line.
    MoveToStart,
    /// Move cursor to end of line.
    MoveToEnd,
    /// Enter: submit the current input.
    Submit,
    /// Ctrl+C: interrupt the current operation.
    Interrupt,
    /// Tab: trigger completion.
    TabComplete,
    /// Up arrow: navigate history backward.
    HistoryUp,
    /// Down arrow: navigate history forward.
    HistoryDown,
    /// Page Up: scroll viewport up.
    PageUp,
    /// Page Down: scroll viewport down.
    PageDown,
    /// Ctrl+K: kill from cursor to end of line.
    KillToEnd,
    /// Ctrl+U: kill from beginning of line to cursor.
    KillToStart,
    /// Ctrl+W: delete word before cursor.
    DeleteWord,
    /// Ctrl+A: select all.
    SelectAll,
    /// Ignore this key event.
    Ignore,
}

/// Convert a key event to an input action.
#[must_use]
pub fn key_event_to_action(event: KeyEvent) -> InputAction {
    match event.code {
        KeyCode::Char(c) => {
            if event.modifiers.contains(KeyModifiers::CONTROL) {
                match c {
                    'c' => InputAction::Interrupt,
                    'a' => InputAction::MoveToStart,
                    'e' => InputAction::MoveToEnd,
                    'k' => InputAction::KillToEnd,
                    'u' => InputAction::KillToStart,
                    'w' => InputAction::DeleteWord,
                    _ => InputAction::Ignore,
                }
            } else {
                InputAction::InsertChar(c)
            }
        }
        KeyCode::Backspace => InputAction::Backspace,
        KeyCode::Delete => InputAction::Delete,
        KeyCode::Left => InputAction::CursorLeft,
        KeyCode::Right => InputAction::CursorRight,
        KeyCode::Enter => InputAction::Submit,
        KeyCode::Tab => InputAction::TabComplete,
        KeyCode::Up => InputAction::HistoryUp,
        KeyCode::Down => InputAction::HistoryDown,
        KeyCode::PageUp => InputAction::PageUp,
        KeyCode::PageDown => InputAction::PageDown,
        KeyCode::Home => InputAction::MoveToStart,
        KeyCode::End => InputAction::MoveToEnd,
        KeyCode::Esc => InputAction::Interrupt,
        _ => InputAction::Ignore,
    }
}

/// The input buffer — managed by the UI thread.
#[derive(Debug, Clone)]
pub struct InputBuffer {
    /// The text content.
    pub text: String,
    /// Cursor position (byte index into text).
    pub cursor: usize,
    /// History of submitted inputs.
    pub history: Vec<String>,
    /// Position within history (0 = not in history, 1 = most recent).
    pub history_pos: usize,
    /// Inline prompt suggestion displayed as ghost text.
    pub suggestion: Option<String>,
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl InputBuffer {
    /// Create a new empty input buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_pos: 0,
            suggestion: None,
        }
    }

    /// Apply an input action to the buffer.
    pub fn apply(&mut self, action: &InputAction) -> Option<String> {
        match action {
            InputAction::InsertChar(c) => {
                self.text.insert(self.cursor, *c);
                self.cursor += c.len_utf8();
                self.history_pos = 0;
                self.suggestion = None;
                None
            }
            InputAction::Backspace => {
                self.suggestion = None;
                if self.cursor > 0 {
                    let prev = self.text[..self.cursor]
                        .char_indices()
                        .last()
                        .map(|(i, c)| (i, c.len_utf8()));
                    if let Some((i, len)) = prev {
                        self.text.drain(i..i + len);
                        self.cursor = i;
                        self.history_pos = 0;
                    }
                }
                None
            }
            InputAction::Delete => {
                self.suggestion = None;
                if self.cursor < self.text.len() {
                    let ch = self.text[self.cursor..].chars().next();
                    if let Some(ch) = ch {
                        let end = self.cursor + ch.len_utf8();
                        self.text.drain(self.cursor..end);
                    }
                }
                None
            }
            InputAction::CursorLeft => {
                if self.cursor > 0 {
                    let prev = self.text[..self.cursor]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i);
                    if let Some(i) = prev {
                        self.cursor = i;
                    }
                }
                None
            }
            InputAction::CursorRight => {
                if self.cursor < self.text.len() {
                    let ch = self.text[self.cursor..].chars().next();
                    if let Some(ch) = ch {
                        self.cursor += ch.len_utf8();
                    }
                }
                None
            }
            InputAction::MoveToStart => {
                self.cursor = 0;
                None
            }
            InputAction::MoveToEnd => {
                self.cursor = self.text.len();
                None
            }
            InputAction::Submit => {
                self.suggestion = None;
                if !self.text.is_empty() {
                    let submitted = self.text.clone();
                    self.history.push(self.text.clone());
                    self.text.clear();
                    self.cursor = 0;
                    self.history_pos = 0;
                    Some(submitted)
                } else {
                    None
                }
            }
            InputAction::HistoryUp => {
                self.suggestion = None;
                if self.history_pos < self.history.len() {
                    self.history_pos += 1;
                    let idx = self.history.len() - self.history_pos;
                    self.text.clone_from(&self.history[idx]);
                    self.cursor = self.text.len();
                }
                None
            }
            InputAction::HistoryDown => {
                self.suggestion = None;
                if self.history_pos > 1 {
                    self.history_pos -= 1;
                    let idx = self.history.len() - self.history_pos;
                    self.text.clone_from(&self.history[idx]);
                    self.cursor = self.text.len();
                } else if self.history_pos == 1 {
                    self.history_pos = 0;
                    self.text.clear();
                    self.cursor = 0;
                }
                None
            }
            InputAction::KillToEnd => {
                self.suggestion = None;
                self.text.truncate(self.cursor);
                None
            }
            InputAction::KillToStart => {
                self.suggestion = None;
                self.text.drain(..self.cursor);
                self.cursor = 0;
                None
            }
            InputAction::DeleteWord => {
                self.suggestion = None;
                if self.cursor > 0 {
                    let before = &self.text[..self.cursor];
                    // Find word boundary: skip whitespace backward, then skip non-whitespace
                    let trimmed = before.trim_end();
                    let last_space = trimmed.rfind(char::is_whitespace);
                    let new_end = last_space.map(|i| i + 1).unwrap_or(0);
                    self.text.drain(new_end..self.cursor);
                    self.cursor = new_end;
                }
                None
            }
            InputAction::PageUp | InputAction::PageDown | InputAction::TabComplete | InputAction::SelectAll | InputAction::Interrupt | InputAction::Ignore => {
                None
            }
        }
    }

    /// Take the submitted text if any.
    pub fn take(&mut self) -> Option<String> {
        self.apply(&InputAction::Submit)
    }

    /// Check if buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Get the display text (for rendering).
    #[must_use]
    pub fn display(&self) -> &str {
        &self.text
    }

    /// Compute the display column (visual width) of the cursor position,
    /// accounting for CJK and other wide characters.
    #[must_use]
    pub fn display_column(&self) -> usize {
        let byte_pos = self.cursor;
        let mut width = 0;
        for c in self.text[..byte_pos.min(self.text.len())].chars() {
            width += UnicodeWidthChar::width(c).unwrap_or(1);
        }
        width
    }

    /// Compute the byte position from a display column, accounting for wide chars.
    #[must_use]
    pub fn byte_from_column(&self, column: usize) -> usize {
        let mut width = 0;
        for (byte_idx, c) in self.text.char_indices() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(1);
            if width + cw > column {
                return byte_idx;
            }
            width += cw;
        }
        self.text.len()
    }

    /// Convert cursor byte offset to display column.
    #[must_use]
    pub fn cursor_display_column(&self) -> usize {
        self.display_column()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(code: KeyCode) -> InputAction {
        key_event_to_action(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl_action(c: char) -> InputAction {
        key_event_to_action(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    #[test]
    fn char_insert() {
        assert_eq!(action(KeyCode::Char('a')), InputAction::InsertChar('a'));
    }

    #[test]
    fn ctrl_c_interrupts() {
        assert_eq!(ctrl_action('c'), InputAction::Interrupt);
    }

    #[test]
    fn ctrl_a_moves_to_start() {
        assert_eq!(ctrl_action('a'), InputAction::MoveToStart);
    }

    #[test]
    fn ctrl_e_moves_to_end() {
        assert_eq!(ctrl_action('e'), InputAction::MoveToEnd);
    }

    #[test]
    fn special_keys() {
        assert_eq!(action(KeyCode::Enter), InputAction::Submit);
        assert_eq!(action(KeyCode::Backspace), InputAction::Backspace);
        assert_eq!(action(KeyCode::Left), InputAction::CursorLeft);
        assert_eq!(action(KeyCode::Right), InputAction::CursorRight);
        assert_eq!(action(KeyCode::Up), InputAction::HistoryUp);
        assert_eq!(action(KeyCode::Down), InputAction::HistoryDown);
    }

    #[test]
    fn buffer_insert_and_submit() {
        let mut buf = InputBuffer::new();
        buf.apply(&InputAction::InsertChar('h'));
        buf.apply(&InputAction::InsertChar('i'));
        assert_eq!(buf.text, "hi");
        assert_eq!(buf.cursor, 2);

        let submitted = buf.take();
        assert_eq!(submitted, Some("hi".into()));
        assert!(buf.text.is_empty());
    }

    #[test]
    fn buffer_backspace() {
        let mut buf = InputBuffer::new();
        buf.apply(&InputAction::InsertChar('a'));
        buf.apply(&InputAction::InsertChar('b'));
        buf.apply(&InputAction::Backspace);
        assert_eq!(buf.text, "a");
        assert_eq!(buf.cursor, 1);
    }

    #[test]
    fn buffer_backspace_at_start_noop() {
        let mut buf = InputBuffer::new();
        buf.apply(&InputAction::Backspace);
        assert!(buf.text.is_empty());
        assert_eq!(buf.cursor, 0);
    }

    #[test]
    fn buffer_cursor_left_right() {
        let mut buf = InputBuffer::new();
        buf.apply(&InputAction::InsertChar('a'));
        buf.apply(&InputAction::InsertChar('b'));
        buf.apply(&InputAction::InsertChar('c'));
        assert_eq!(buf.cursor, 3);

        buf.apply(&InputAction::CursorLeft);
        assert_eq!(buf.cursor, 2);
        buf.apply(&InputAction::CursorLeft);
        assert_eq!(buf.cursor, 1);
        buf.apply(&InputAction::CursorRight);
        assert_eq!(buf.cursor, 2);
    }

    #[test]
    fn buffer_move_to_start_end() {
        let mut buf = InputBuffer::new();
        buf.apply(&InputAction::InsertChar('a'));
        buf.apply(&InputAction::InsertChar('b'));
        buf.apply(&InputAction::MoveToStart);
        assert_eq!(buf.cursor, 0);
        buf.apply(&InputAction::MoveToEnd);
        assert_eq!(buf.cursor, 2);
    }

    #[test]
    fn buffer_history_navigation() {
        let mut buf = InputBuffer::new();
        buf.apply(&InputAction::InsertChar('h'));
        buf.apply(&InputAction::InsertChar('i'));
        buf.take(); // submit "hi"
        buf.apply(&InputAction::InsertChar('h'));
        buf.apply(&InputAction::InsertChar('e'));
        buf.apply(&InputAction::InsertChar('l'));
        buf.apply(&InputAction::InsertChar('l'));
        buf.apply(&InputAction::InsertChar('o'));
        buf.take(); // submit "hello"

        // Navigate up in history
        buf.apply(&InputAction::HistoryUp);
        assert_eq!(buf.text, "hello");
        buf.apply(&InputAction::HistoryUp);
        assert_eq!(buf.text, "hi");

        // Navigate down
        buf.apply(&InputAction::HistoryDown);
        assert_eq!(buf.text, "hello");
        buf.apply(&InputAction::HistoryDown);
        assert_eq!(buf.text, "");
    }

    #[test]
    fn buffer_kill_to_end() {
        let mut buf = InputBuffer::new();
        buf.apply(&InputAction::InsertChar('h'));
        buf.apply(&InputAction::InsertChar('i'));
        buf.apply(&InputAction::CursorLeft);
        buf.apply(&InputAction::KillToEnd);
        assert_eq!(buf.text, "h");
    }

    #[test]
    fn buffer_kill_to_start() {
        let mut buf = InputBuffer::new();
        buf.apply(&InputAction::InsertChar('h'));
        buf.apply(&InputAction::InsertChar('e'));
        buf.apply(&InputAction::InsertChar('l'));
        buf.apply(&InputAction::InsertChar('l'));
        buf.apply(&InputAction::InsertChar('o'));
        buf.apply(&InputAction::MoveToStart);
        buf.apply(&InputAction::CursorRight);
        buf.apply(&InputAction::KillToStart);
        assert_eq!(buf.text, "ello");
        assert_eq!(buf.cursor, 0);
    }

    #[test]
    fn buffer_delete_word() {
        let mut buf = InputBuffer::new();
        buf.apply(&InputAction::InsertChar('h'));
        buf.apply(&InputAction::InsertChar('e'));
        buf.apply(&InputAction::InsertChar('l'));
        buf.apply(&InputAction::InsertChar('l'));
        buf.apply(&InputAction::InsertChar('o'));
        buf.apply(&InputAction::InsertChar(' '));
        buf.apply(&InputAction::InsertChar('w'));
        buf.apply(&InputAction::InsertChar('o'));
        buf.apply(&InputAction::InsertChar('r'));
        buf.apply(&InputAction::InsertChar('l'));
        buf.apply(&InputAction::InsertChar('d'));
        buf.apply(&InputAction::DeleteWord);
        assert_eq!(buf.text, "hello ");
    }

    #[test]
    fn buffer_delete_at_end_noop() {
        let mut buf = InputBuffer::new();
        buf.apply(&InputAction::InsertChar('a'));
        buf.apply(&InputAction::Delete);
        assert_eq!(buf.text, "a");
    }

    #[test]
    fn empty_submit_returns_none() {
        let mut buf = InputBuffer::new();
        assert!(buf.take().is_none());
    }

    #[test]
    fn display_column_handles_cjk() {
        let mut buf = InputBuffer::new();
        buf.apply(&InputAction::InsertChar('中'));
        buf.apply(&InputAction::InsertChar('文'));
        // "中文" has display width 4 (each char is 2 columns)
        assert_eq!(buf.display_column(), 4);
        buf.apply(&InputAction::CursorLeft);
        // After moving left, cursor is at byte position for '中', display column 2
        assert_eq!(buf.display_column(), 2);
    }

    #[test]
    fn byte_from_column_handles_cjk() {
        let mut buf = InputBuffer::new();
        buf.apply(&InputAction::InsertChar('a'));
        buf.apply(&InputAction::InsertChar('中'));
        buf.apply(&InputAction::InsertChar('b'));
        // "a中b" — byte positions: a=0, 中=1, b=4; display columns: a=1, 中=3, b=4
        assert_eq!(buf.byte_from_column(0), 0);  // before 'a'
        assert_eq!(buf.byte_from_column(1), 1);  // before '中'
        assert_eq!(buf.byte_from_column(2), 1);  // middle of '中', rounds down
        assert_eq!(buf.byte_from_column(3), 4);  // before 'b'
        assert_eq!(buf.byte_from_column(4), 5);  // after 'b' (text.len())
    }
}
