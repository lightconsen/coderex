//! Input handling — keyboard event processing and input buffer editing.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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

/// Drop the key events that are not a key going down.
///
/// Windows reports releases unconditionally, and the kitty keyboard protocol
/// reports them when `REPORT_EVENT_TYPES` is on. Applying a release as if it
/// were a press types every character twice, so the filter belongs at the
/// boundary where raw events come in.
///
/// `Repeat` is kept: an autorepeating key is a real keypress.
#[must_use]
pub fn key_press(event: KeyEvent) -> Option<KeyEvent> {
    (event.kind != KeyEventKind::Release).then_some(event)
}

/// Convert a key event to an input action.
#[must_use]
pub fn key_event_to_action(event: KeyEvent) -> InputAction {
    let Some(event) = key_press(event) else {
        return InputAction::Ignore;
    };

    match event.code {
        KeyCode::Char(c) => {
            if event.modifiers.contains(KeyModifiers::CONTROL) {
                // Control chords are case-insensitive. The kitty keyboard
                // protocol may report `Ctrl+Shift+C` as either `'c'` or `'C'`
                // with CONTROL depending on which enhancement flags are on,
                // and both are the same key to the user.
                match c.to_ascii_lowercase() {
                    'c' => InputAction::Interrupt,
                    'a' => InputAction::MoveToStart,
                    'e' => InputAction::MoveToEnd,
                    'k' => InputAction::KillToEnd,
                    'u' => InputAction::KillToStart,
                    'w' => InputAction::DeleteWord,
                    _ => InputAction::Ignore,
                }
            } else if c.is_control() {
                // Under CSI-u a control character can arrive as `Char` where
                // the legacy encoding delivered a named key: `Ctrl+J` is
                // `Char('\n')` either way. A single-line buffer must never take
                // one — it would break the one-row input invariant.
                InputAction::Ignore
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
    /// Visual-mode selection anchor (byte index). `None` when no selection is
    /// active. The other end of the selection is always `cursor`.
    pub visual_anchor: Option<usize>,
    /// Whether the active Visual selection is linewise (`V`) rather than
    /// characterwise (`v`).
    pub visual_linewise: bool,
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl InputBuffer {
    /// Create a new empty input buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_pos: 0,
            suggestion: None,
            visual_anchor: None,
            visual_linewise: false,
        }
    }

    /// Apply an input action to the buffer.
    pub fn apply(&mut self, action: &InputAction) -> Option<String> {
        match action {
            InputAction::InsertChar(c) => {
                // The single-line buffer invariant is enforced here as well as
                // at the event boundary. Callers that build an `InsertChar`
                // directly (the REPL's Vim insert path, a yank/paste) must not
                // be able to smuggle in a newline or another control character
                // that would break the one-row input.
                if c.is_control() {
                    return None;
                }
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
                self.clear_visual_selection();
                if self.text.is_empty() {
                    None
                } else {
                    let submitted = self.text.clone();
                    self.history.push(self.text.clone());
                    self.text.clear();
                    self.cursor = 0;
                    self.history_pos = 0;
                    Some(submitted)
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
                    let new_end = last_space.map_or(0, |i| i + 1);
                    self.text.drain(new_end..self.cursor);
                    self.cursor = new_end;
                }
                None
            }
            InputAction::PageUp
            | InputAction::PageDown
            | InputAction::TabComplete
            | InputAction::SelectAll
            | InputAction::Interrupt
            | InputAction::Ignore => None,
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

    /// Begin a Visual selection anchored at the cursor.
    ///
    /// Switching between `v` and `V` keeps the existing anchor; only the
    /// characterwise/linewise flag changes.
    pub fn start_visual(&mut self, linewise: bool) {
        if self.visual_anchor.is_none() {
            self.visual_anchor = Some(self.cursor);
        }
        self.visual_linewise = linewise;
        self.suggestion = None;
    }

    /// Drop the Visual selection without editing the buffer.
    pub const fn clear_visual_selection(&mut self) {
        self.visual_anchor = None;
        self.visual_linewise = false;
    }

    /// The selected byte range, as `[start, end)`, or `None` when no
    /// non-empty range is selected.
    ///
    /// A linewise selection always covers the whole buffer: the input is a
    /// single logical line, so `V` selects all of it. Characterwise
    /// selections run between the anchor and the insertion-point cursor.
    #[must_use]
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.floor_char_boundary(self.visual_anchor?);
        let len = self.text.len();
        if self.visual_linewise {
            return (len > 0).then_some((0, len));
        }
        let cursor = self.floor_char_boundary(self.cursor);
        let start = anchor.min(cursor);
        let end = anchor.max(cursor);
        (start < end).then_some((start, end))
    }

    /// Round `index` down to a UTF-8 character boundary.
    ///
    /// Cursor and anchor are maintained on boundaries already; this keeps the
    /// slicing in `selection_range` safe if a caller writes the public fields
    /// directly.
    fn floor_char_boundary(&self, index: usize) -> usize {
        let mut index = index.min(self.text.len());
        while index > 0 && !self.text.is_char_boundary(index) {
            index -= 1;
        }
        index
    }

    /// The selected text, if any.
    #[must_use]
    pub fn selection_text(&self) -> Option<&str> {
        let (start, end) = self.selection_range()?;
        self.text.get(start..end)
    }

    /// Copy the selection into a string and leave Visual mode.
    ///
    /// Like Vim, the cursor moves to the start of the yanked range.
    pub fn yank_selection(&mut self) -> Option<String> {
        let range = self.selection_range();
        let yanked = range
            .and_then(|(start, end)| self.text.get(start..end))
            .map(str::to_string);
        if let Some((start, _)) = range {
            self.cursor = start;
            self.history_pos = 0;
        }
        self.suggestion = None;
        self.clear_visual_selection();
        yanked
    }

    /// Delete the selection and leave Visual mode.
    ///
    /// The cursor lands where the deleted text began.
    pub fn delete_selection(&mut self) -> Option<String> {
        let range = self.selection_range();
        let deleted = range.map(|(start, end)| {
            let deleted: String = self.text.drain(start..end).collect();
            self.cursor = start;
            deleted
        });
        if range.is_some() {
            self.history_pos = 0;
        }
        self.suggestion = None;
        self.clear_visual_selection();
        deleted
    }

    /// Move to the start of the next word (Vim `w`).
    ///
    /// Word boundaries follow character classes: a run of whitespace, a run of
    /// alphanumeric/underscore characters, and a run of other punctuation each
    /// count as their own token.
    pub fn word_forward(&mut self) {
        let len = self.text.len();
        if self.cursor >= len {
            return;
        }
        let mut chars = self.text[self.cursor..].char_indices().peekable();
        let Some(&(_, first)) = chars.peek() else {
            return;
        };
        let class = char_class(first);
        while let Some(&(_, c)) = chars.peek() {
            if char_class(c) != class {
                break;
            }
            chars.next();
        }
        // The previous run may have been whitespace; skip any that remains so
        // the cursor lands on a word rather than the space before it.
        while let Some(&(_, c)) = chars.peek() {
            if !c.is_whitespace() {
                break;
            }
            chars.next();
        }
        self.cursor = match chars.peek() {
            Some(&(offset, _)) => self.cursor + offset,
            None => len,
        };
        self.suggestion = None;
    }

    /// Move to the start of the previous word (Vim `b`).
    pub fn word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Start at the character before the insertion point, then walk back
        // over whitespace and the preceding same-class run.
        let mut idx = self
            .text
            .get(..self.cursor)
            .and_then(|before| before.char_indices().last())
            .map_or(0, |(i, _)| i);
        while idx > 0 {
            let Some(c) = self.text.get(idx..).and_then(|s| s.chars().next()) else {
                break;
            };
            if !c.is_whitespace() {
                break;
            }
            idx = self
                .text
                .get(..idx)
                .and_then(|before| before.char_indices().last())
                .map_or(0, |(i, _)| i);
        }
        let Some(class) = self
            .text
            .get(idx..)
            .and_then(|s| s.chars().next())
            .map(char_class)
        else {
            return;
        };
        while idx > 0 {
            let Some(prev_idx) = self
                .text
                .get(..idx)
                .and_then(|before| before.char_indices().last())
                .map(|(i, _)| i)
            else {
                break;
            };
            let same_class = self
                .text
                .get(prev_idx..)
                .and_then(|s| s.chars().next())
                .is_some_and(|c| char_class(c) == class);
            if !same_class {
                break;
            }
            idx = prev_idx;
        }
        self.cursor = idx;
        self.suggestion = None;
    }

    /// Move to the end of the current or next word (Vim `e`).
    ///
    /// The cursor is an insertion point, so the returned position is just past
    /// the word's final character — the position at which a Visual selection
    /// includes that character.
    pub fn word_end(&mut self) {
        let len = self.text.len();
        if self.cursor >= len {
            return;
        }
        let mut chars = self.text[self.cursor..].char_indices().peekable();
        while let Some(&(_, c)) = chars.peek() {
            if !c.is_whitespace() {
                break;
            }
            chars.next();
        }
        let Some(&(_, first)) = chars.peek() else {
            self.cursor = len;
            return;
        };
        let class = char_class(first);
        let mut end = self.cursor;
        while let Some(&(offset, c)) = chars.peek() {
            if char_class(c) != class {
                break;
            }
            chars.next();
            end = self.cursor + offset + c.len_utf8();
        }
        self.cursor = end;
        self.suggestion = None;
    }
}

/// Vim's notion of a word character, simplified: letters, digits, underscore.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Character class used by the word motions: whitespace, word, punctuation.
fn char_class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if is_word_char(c) {
        1
    } else {
        2
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

    fn kind_action(code: KeyCode, kind: KeyEventKind) -> InputAction {
        key_event_to_action(KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind))
    }

    /// A key release is not a keypress. Windows always reports releases, and
    /// the kitty keyboard protocol reports them when `REPORT_EVENT_TYPES` is
    /// pushed; without this filter every character is typed twice.
    #[test]
    fn key_release_is_not_a_keypress() {
        assert_eq!(
            kind_action(KeyCode::Char('a'), KeyEventKind::Press),
            InputAction::InsertChar('a')
        );
        assert_eq!(
            kind_action(KeyCode::Char('a'), KeyEventKind::Release),
            InputAction::Ignore
        );
        // Autorepeat is a real keypress and must still insert.
        assert_eq!(
            kind_action(KeyCode::Char('a'), KeyEventKind::Repeat),
            InputAction::InsertChar('a')
        );
        assert_eq!(
            kind_action(KeyCode::Enter, KeyEventKind::Release),
            InputAction::Ignore
        );
    }

    #[test]
    fn key_press_filters_releases() {
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert!(key_press(release).is_none());
        assert!(key_press(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)).is_some());
    }

    /// Control chords are case-insensitive. Depending on which kitty keyboard
    /// enhancement flags are active, `Ctrl+Shift+C` is reported as either
    /// `'c'` or `'C'` with CONTROL; both mean the same key.
    #[test]
    fn control_chords_ignore_letter_case() {
        assert_eq!(ctrl_action('C'), InputAction::Interrupt);
        assert_eq!(ctrl_action('A'), InputAction::MoveToStart);
        assert_eq!(ctrl_action('E'), InputAction::MoveToEnd);
        assert_eq!(ctrl_action('K'), InputAction::KillToEnd);
        assert_eq!(ctrl_action('U'), InputAction::KillToStart);
        assert_eq!(ctrl_action('W'), InputAction::DeleteWord);
    }

    /// Disambiguation is the point of the protocol: keys that used to be the
    /// same bytes become distinct, and each has to land on its own action.
    #[test]
    fn disambiguated_keys_stay_distinct() {
        // Esc alone still interrupts…
        assert_eq!(action(KeyCode::Esc), InputAction::Interrupt);
        // …but `Ctrl+[` is no longer Esc, and must not interrupt.
        assert_eq!(ctrl_action('['), InputAction::Ignore);

        // Tab still completes; `Ctrl+I` is a different key.
        assert_eq!(action(KeyCode::Tab), InputAction::TabComplete);
        assert_eq!(ctrl_action('i'), InputAction::Ignore);

        // Enter still submits; `Ctrl+M` is a different key.
        assert_eq!(action(KeyCode::Enter), InputAction::Submit);
        assert_eq!(ctrl_action('m'), InputAction::Ignore);
    }

    /// A control character must never reach the single-line buffer, whichever
    /// encoding delivered it.
    #[test]
    fn control_characters_are_never_inserted() {
        assert_eq!(action(KeyCode::Char('\n')), InputAction::Ignore);
        assert_eq!(action(KeyCode::Char('\r')), InputAction::Ignore);
        assert_eq!(action(KeyCode::Char('\u{1b}')), InputAction::Ignore);
        // Ordinary text is unaffected.
        assert_eq!(action(KeyCode::Char('a')), InputAction::InsertChar('a'));
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

    /// The buffer itself is the last line of defence: even a caller that
    /// constructs `InsertChar` directly (rather than going through
    /// `key_event_to_action`) cannot put a control character in a single-line
    /// input.
    #[test]
    fn buffer_never_accepts_control_characters() {
        let mut buf = InputBuffer::new();
        for control in ['\n', '\r', '\u{1b}', '\u{1}', '\u{7f}'] {
            buf.apply(&InputAction::InsertChar(control));
        }
        assert!(buf.text.is_empty(), "a control character was inserted");
        assert_eq!(buf.cursor, 0);

        buf.apply(&InputAction::InsertChar('a'));
        buf.apply(&InputAction::InsertChar('\n'));
        buf.apply(&InputAction::InsertChar('b'));
        assert_eq!(buf.text, "ab");
        assert_eq!(buf.cursor, 2);
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
        assert_eq!(buf.byte_from_column(0), 0); // before 'a'
        assert_eq!(buf.byte_from_column(1), 1); // before '中'
        assert_eq!(buf.byte_from_column(2), 1); // middle of '中', rounds down
        assert_eq!(buf.byte_from_column(3), 4); // before 'b'
        assert_eq!(buf.byte_from_column(4), 5); // after 'b' (text.len())
    }

    fn buffer_with(text: &str) -> InputBuffer {
        let mut buf = InputBuffer::new();
        buf.text = text.to_string();
        buf.cursor = text.len();
        buf
    }

    #[test]
    fn start_visual_anchors_at_cursor() {
        let mut buf = buffer_with("hello");
        buf.cursor = 2;
        buf.start_visual(false);
        assert_eq!(buf.visual_anchor, Some(2));
        assert!(!buf.visual_linewise);
        // No characters between anchor and cursor yet.
        assert_eq!(buf.selection_range(), None);

        buf.cursor = 4;
        assert_eq!(buf.selection_range(), Some((2, 4)));
        assert_eq!(buf.selection_text(), Some("ll"));
    }

    #[test]
    fn selection_range_works_backwards() {
        let mut buf = buffer_with("hello");
        buf.cursor = 4;
        buf.start_visual(false);
        buf.cursor = 1;
        assert_eq!(buf.selection_range(), Some((1, 4)));
        assert_eq!(buf.selection_text(), Some("ell"));
    }

    #[test]
    fn selection_handles_multibyte_utf8() {
        let mut buf = buffer_with("a中文b");
        // Bytes: a=0, 中=1..4, 文=4..7, b=7.
        buf.cursor = 1;
        buf.start_visual(false);
        buf.cursor = 7;
        assert_eq!(buf.selection_range(), Some((1, 7)));
        assert_eq!(buf.selection_text(), Some("中文"));

        let yanked = buf.yank_selection();
        assert_eq!(yanked.as_deref(), Some("中文"));
        // Yanking leaves the text intact and clears the selection.
        assert_eq!(buf.text, "a中文b");
        assert_eq!(buf.visual_anchor, None);
    }

    #[test]
    fn linewise_selection_covers_the_whole_buffer() {
        let mut buf = buffer_with("hello world");
        buf.cursor = 3;
        buf.start_visual(true);
        assert!(buf.visual_linewise);
        assert_eq!(buf.selection_range(), Some((0, 11)));
        assert_eq!(buf.selection_text(), Some("hello world"));
    }

    #[test]
    fn switching_visual_kind_keeps_anchor() {
        let mut buf = buffer_with("hello");
        buf.cursor = 2;
        buf.start_visual(false);
        buf.cursor = 4;
        buf.start_visual(true);
        assert_eq!(buf.visual_anchor, Some(2));
        assert_eq!(buf.selection_range(), Some((0, 5)));
    }

    #[test]
    fn delete_selection_removes_range_and_moves_cursor() {
        let mut buf = buffer_with("hello world");
        buf.cursor = 0;
        buf.start_visual(false);
        buf.cursor = 5;
        assert_eq!(buf.delete_selection().as_deref(), Some("hello"));
        assert_eq!(buf.text, " world");
        assert_eq!(buf.cursor, 0);
        assert_eq!(buf.visual_anchor, None);
        // The rest of the buffer is untouched and still editable.
        buf.cursor = buf.text.len();
        buf.apply(&InputAction::InsertChar('!'));
        assert_eq!(buf.text, " world!");
    }

    #[test]
    fn yank_selection_moves_cursor_to_start() {
        let mut buf = buffer_with("one two");
        buf.cursor = 4;
        buf.start_visual(false);
        buf.cursor = 7;
        assert_eq!(buf.yank_selection().as_deref(), Some("two"));
        assert_eq!(buf.cursor, 4);
        assert_eq!(buf.text, "one two");
    }

    #[test]
    fn word_motions_match_vim_boundaries() {
        let mut buf = buffer_with("hello world");
        buf.cursor = 0;
        buf.word_forward();
        assert_eq!(buf.cursor, 6); // start of "world"
        buf.word_backward();
        assert_eq!(buf.cursor, 0);
        buf.word_end();
        assert_eq!(buf.cursor, 5); // just past "hello"
        buf.word_forward();
        assert_eq!(buf.cursor, 6);
        buf.word_end();
        assert_eq!(buf.cursor, 11); // end of buffer
    }

    #[test]
    fn word_motions_handle_multibyte_and_punctuation() {
        let mut buf = buffer_with("你好 world");
        buf.cursor = 0;
        buf.word_end();
        // "你好" is one word run; the cursor sits just past 好 (byte 6).
        assert_eq!(buf.cursor, 6);
        buf.word_forward();
        assert_eq!(buf.cursor, 7); // start of "world" after the space
        buf.word_backward();
        assert_eq!(buf.cursor, 0);

        let mut buf = buffer_with("foo.bar");
        buf.cursor = 0;
        buf.word_end();
        assert_eq!(buf.cursor, 3); // end of "foo"
        buf.word_forward();
        assert_eq!(buf.cursor, 4); // punctuation is its own token
        buf.word_end();
        assert_eq!(buf.cursor, 7); // past "bar"
    }

    #[test]
    fn word_motion_does_not_split_utf8() {
        let mut buf = buffer_with("a中 b");
        buf.cursor = 0;
        buf.word_end();
        assert_eq!(buf.cursor, 4); // past 中, before the space
        assert!(buf.text.is_char_boundary(buf.cursor));
    }

    #[test]
    fn visual_selection_survives_history_state() {
        let mut buf = buffer_with("draft");
        buf.history.push("older".into());
        buf.cursor = 5;
        buf.start_visual(false);
        buf.cursor = 0;
        assert_eq!(buf.selection_text(), Some("draft"));
        // History position is independent of selection state.
        assert_eq!(buf.history_pos, 0);
    }
}
