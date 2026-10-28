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
///
/// The buffer holds `\n`-separated text: it is multi-line so that Vim's
/// line-wise operations (`o`, `O`, `dd`, `cc`, `yy`, `>>`, `j`/`k`, `0`/`$`,
/// `gg`/`G`) act on lines. A buffer with no newline is a single line, and every
/// line-relative operation degenerates to the whole buffer there, so one-line
/// input behaves exactly as a single-line buffer always did.
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
                // Line-relative: on a single-line buffer this is the start of
                // the buffer, exactly as before.
                self.cursor = self.line_bounds_at(self.cursor).0;
                None
            }
            InputAction::MoveToEnd => {
                self.cursor = self.line_bounds_at(self.cursor).1;
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
                let (_, line_end) = self.line_bounds_at(self.cursor);
                self.text.drain(self.cursor..line_end);
                None
            }
            InputAction::KillToStart => {
                self.suggestion = None;
                let (line_start, _) = self.line_bounds_at(self.cursor);
                self.text.drain(line_start..self.cursor);
                self.cursor = line_start;
                None
            }
            InputAction::DeleteWord => {
                self.suggestion = None;
                // Ctrl+W never eats a line break: it deletes back to the start
                // of the word, at most to the start of the current line.
                let (line_start, _) = self.line_bounds_at(self.cursor);
                if self.cursor > line_start {
                    let before = &self.text[line_start..self.cursor];
                    // Find word boundary: skip whitespace backward, then skip non-whitespace
                    let trimmed = before.trim_end();
                    let last_space = trimmed.rfind(char::is_whitespace);
                    let new_end = line_start + last_space.map_or(0, |i| i + 1);
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
    pub const fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Get the display text (for rendering).
    #[must_use]
    pub fn display(&self) -> &str {
        &self.text
    }

    /// Compute the display column (visual width) of the cursor within its
    /// line, accounting for CJK and other wide characters.
    ///
    /// The column is measured from the start of the cursor's line, so a
    /// single-line buffer reports the same value it always did.
    #[must_use]
    pub fn display_column(&self) -> usize {
        let (line_start, _) = self.line_bounds_at(self.cursor);
        display_width(&self.text[line_start..self.cursor.min(self.text.len())])
    }

    /// Compute the byte position within the cursor's line from a display
    /// column, accounting for wide chars.
    #[must_use]
    pub fn byte_from_column(&self, column: usize) -> usize {
        let (line_start, line_end) = self.line_bounds_at(self.cursor);
        line_start + byte_at_column(&self.text[line_start..line_end], column)
    }

    /// Convert cursor byte offset to display column.
    #[must_use]
    pub fn cursor_display_column(&self) -> usize {
        self.display_column()
    }

    /// Number of lines in the buffer. A buffer with no newline is one line,
    /// including the empty buffer.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.text.bytes().filter(|b| *b == b'\n').count() + 1
    }

    /// The byte range `[start, end)` of the line containing `byte`, where
    /// `end` excludes the terminating newline.
    ///
    /// A `byte` sitting on a newline belongs to the line before it, so the
    /// cursor — which is an insertion point — is always inside the line it
    /// looks like it is on.
    #[must_use]
    pub fn line_bounds_at(&self, byte: usize) -> (usize, usize) {
        let byte = byte.min(self.text.len());
        let start = self.text[..byte].rfind('\n').map_or(0, |i| i + 1);
        let end = self.text[byte..]
            .find('\n')
            .map_or(self.text.len(), |i| byte + i);
        (start, end)
    }

    /// The byte range of the cursor's line.
    #[must_use]
    pub fn current_line(&self) -> (usize, usize) {
        self.line_bounds_at(self.cursor)
    }

    /// Move the cursor one line up (`delta < 0`) or down, keeping its display
    /// column where the target line is long enough. Returns whether the cursor
    /// moved: on a single-line buffer there is never anywhere to go.
    pub fn move_line(&mut self, delta: isize) -> bool {
        let (start, end) = self.current_line();
        if delta < 0 {
            if start == 0 {
                return false;
            }
            let column = display_width(&self.text[start..self.cursor]);
            let previous_end = start - 1;
            let previous_start = self.text[..previous_end].rfind('\n').map_or(0, |i| i + 1);
            let offset = byte_at_column(&self.text[previous_start..previous_end], column);
            self.cursor = previous_start + offset;
        } else {
            if end >= self.text.len() {
                return false;
            }
            let column = display_width(&self.text[start..self.cursor]);
            let next_start = end + 1;
            let next_end = self.text[next_start..]
                .find('\n')
                .map_or(self.text.len(), |i| next_start + i);
            let offset = byte_at_column(&self.text[next_start..next_end], column);
            self.cursor = next_start + offset;
        }
        self.suggestion = None;
        true
    }

    /// Move the cursor to the start of the buffer (`gg`).
    pub const fn move_to_buffer_start(&mut self) {
        self.cursor = 0;
    }

    /// Move the cursor to the end of the buffer (`G`).
    pub const fn move_to_buffer_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Open a new line below the current one, leaving the cursor at its start
    /// (Vim `o`).
    pub fn open_line_below(&mut self) {
        let (_, line_end) = self.current_line();
        self.text.insert(line_end, '\n');
        self.cursor = line_end + 1;
        self.history_pos = 0;
        self.suggestion = None;
    }

    /// Open a new line above the current one, leaving the cursor at its start
    /// (Vim `O`).
    pub fn open_line_above(&mut self) {
        let (line_start, _) = self.current_line();
        self.text.insert(line_start, '\n');
        self.cursor = line_start;
        self.history_pos = 0;
        self.suggestion = None;
    }

    /// Delete `count` lines starting at the current one, including their line
    /// breaks (Vim `dd`). The cursor lands at the start of the line that takes
    /// their place.
    pub fn delete_lines(&mut self, count: usize) {
        let (start, last_end) = self.line_span(count);
        let (remove_start, remove_end) = if last_end < self.text.len() {
            (start, last_end + 1)
        } else if start > 0 {
            (start - 1, last_end)
        } else {
            (start, last_end)
        };
        self.text.drain(remove_start..remove_end);
        self.cursor = self.line_bounds_at(remove_start.min(self.text.len())).0;
        self.history_pos = 0;
        self.suggestion = None;
    }

    /// Clear the content of `count` lines starting at the current one but keep
    /// a single empty line in their place, leaving the cursor at its start
    /// (Vim `cc`, `S`).
    pub fn change_lines(&mut self, count: usize) {
        let (start, last_end) = self.line_span(count);
        self.text.drain(start..last_end);
        self.cursor = start;
        self.history_pos = 0;
        self.suggestion = None;
    }

    /// The current line's text, with a trailing newline when it is not the
    /// last line (Vim `yy`). The second value reports whether the register
    /// holds whole lines, so a paste can re-insert it as lines.
    ///
    /// A single-line buffer yanks its whole text without a newline: a
    /// linewise paste would otherwise change what `yy`+`p` does today.
    #[must_use]
    pub fn yank_lines(&self, count: usize) -> (String, bool) {
        if self.line_count() <= 1 {
            return (self.text.clone(), false);
        }
        let (start, last_end) = self.line_span(count);
        let end = if last_end < self.text.len() {
            last_end + 1
        } else {
            last_end
        };
        (self.text[start..end].to_string(), true)
    }

    /// Insert `text` at the cursor (Vim `p`). Unlike typing, this keeps
    /// newlines: the register can hold whole lines.
    pub fn paste(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.history_pos = 0;
        self.suggestion = None;
    }

    /// Paste a linewise register as whole lines below (`below`) or above the
    /// current line (Vim `p`/`P` on a linewise register).
    pub fn paste_linewise(&mut self, text: &str, count: usize, below: bool) {
        let mut block = String::new();
        for _ in 0..count {
            block.push_str(text);
        }
        if !block.ends_with('\n') {
            block.push('\n');
        }
        let (start, end) = self.current_line();
        let (at, prefix) = if below && !self.text.is_empty() {
            if end < self.text.len() {
                (end + 1, "")
            } else {
                (end, "\n")
            }
        } else {
            (start, "")
        };
        let cursor = at + prefix.len();
        self.text.insert_str(at, prefix);
        self.text.insert_str(cursor, &block);
        self.cursor = cursor;
        self.history_pos = 0;
        self.suggestion = None;
    }

    /// The byte range from the start of the current line through the end of
    /// the `count`th line (excluding its newline), clamped to the buffer.
    fn line_span(&self, count: usize) -> (usize, usize) {
        let (start, mut end) = self.current_line();
        for _ in 1..count.max(1) {
            if end >= self.text.len() {
                break;
            }
            let next = end + 1;
            end = self.text[next..]
                .find('\n')
                .map_or(self.text.len(), |i| next + i);
        }
        (start, end)
    }

    /// Start offsets of the lines an indent/outdent affects: the cursor's line
    /// in normal mode, every line the Visual selection spans otherwise.
    fn affected_line_starts(&self) -> Vec<usize> {
        let Some(anchor) = self.visual_anchor else {
            return vec![self.current_line().0];
        };
        let anchor_start = self.line_bounds_at(anchor).0;
        let cursor_start = self.current_line().0;
        let (first, last) = (
            anchor_start.min(cursor_start),
            anchor_start.max(cursor_start),
        );
        let mut starts = Vec::new();
        let mut start = first;
        loop {
            starts.push(start);
            if start >= last {
                break;
            }
            let (_, end) = self.line_bounds_at(start);
            if end >= self.text.len() {
                break;
            }
            start = end + 1;
        }
        starts
    }

    /// Split the buffer into display rows for rendering at `wrap_width` display
    /// columns, returning each row's byte range.
    ///
    /// Rows break mid-word, as Vim's do, and a row that exactly fills the width
    /// is followed by an empty one so the cursor always has a cell to sit in.
    /// A logical line break always starts a new row, so the row list never
    /// merges two lines.
    #[must_use]
    pub fn display_rows(&self, wrap_width: usize) -> Vec<(usize, usize)> {
        let wrap = wrap_width.max(1);
        let mut rows: Vec<(usize, usize)> = Vec::new();
        let mut line_start = 0usize;
        loop {
            let line_end = self.text[line_start..]
                .find('\n')
                .map_or(self.text.len(), |i| line_start + i);
            let mut row_start = line_start;
            let mut width = 0usize;
            for (offset, c) in self.text[line_start..line_end].char_indices() {
                let char_width = UnicodeWidthChar::width(c).unwrap_or(1);
                if width > 0 && width + char_width > wrap {
                    rows.push((row_start, line_start + offset));
                    row_start = line_start + offset;
                    width = 0;
                }
                width += char_width;
            }
            rows.push((row_start, line_end));
            if line_end > line_start && width == wrap {
                rows.push((line_end, line_end));
            }
            if line_end >= self.text.len() {
                break;
            }
            line_start = line_end + 1;
        }
        rows
    }

    /// The cursor's `(row, column)` within [`Self::display_rows`] at the same
    /// wrap width, both in display units.
    #[must_use]
    pub fn display_cursor(&self, wrap_width: usize) -> (usize, usize) {
        let rows = self.display_rows(wrap_width);
        let cursor = self.cursor.min(self.text.len());
        let mut row = 0usize;
        for (index, &(start, _)) in rows.iter().enumerate() {
            if start > cursor {
                break;
            }
            row = index;
        }
        let (start, _) = rows[row];
        (row, display_width(&self.text[start..cursor.max(start)]))
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
    /// A linewise selection covers every whole line between the anchor's line
    /// and the cursor's line, including the trailing newline of the last one.
    /// On a single-line buffer that is the whole text, exactly as before.
    /// Characterwise selections run between the anchor and the insertion-point
    /// cursor.
    #[must_use]
    pub fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.floor_char_boundary(self.visual_anchor?);
        if self.visual_linewise {
            let cursor = self.floor_char_boundary(self.cursor);
            let (anchor_start, anchor_end) = self.line_bounds_at(anchor);
            let (cursor_start, cursor_end) = self.line_bounds_at(cursor);
            let start = anchor_start.min(cursor_start);
            let last_end = anchor_end.max(cursor_end);
            let end = if last_end < self.text.len() {
                last_end + 1
            } else {
                last_end
            };
            return (start < end).then_some((start, end));
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
        self.cursor = self.word_forward_from(self.cursor);
        self.suggestion = None;
    }

    /// The byte offset `w` would move to from `from`. Newlines are whitespace,
    /// so a word motion crosses lines the way Vim's does.
    fn word_forward_from(&self, from: usize) -> usize {
        let len = self.text.len();
        if from >= len {
            return len;
        }
        let mut chars = self.text[from..].char_indices().peekable();
        let Some(&(_, first)) = chars.peek() else {
            return len;
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
        match chars.peek() {
            Some(&(offset, _)) => from + offset,
            None => len,
        }
    }

    /// Move to the start of the previous word (Vim `b`).
    pub fn word_backward(&mut self) {
        self.cursor = self.word_backward_from(self.cursor);
        self.suggestion = None;
    }

    /// The byte offset `b` would move to from `from`.
    fn word_backward_from(&self, from: usize) -> usize {
        if from == 0 {
            return 0;
        }
        // Start at the character before the insertion point, then walk back
        // over whitespace and the preceding same-class run.
        let mut idx = self
            .text
            .get(..from)
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
            return idx;
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
        idx
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

    /// Delete from the cursor to the end of the current word run, and return
    /// the deleted text (Vim `cw`).
    ///
    /// Unlike [`Self::word_end`], this does not skip leading whitespace: `cw`
    /// on whitespace deletes the whitespace run, and on a word deletes the
    /// word. The cursor stays where the deletion began, ready for insert mode.
    pub fn delete_word_end(&mut self) -> String {
        if self.cursor >= self.text.len() {
            return String::new();
        }
        let class = self.text[self.cursor..].chars().next().map(char_class);
        let mut end = self.cursor;
        for (offset, c) in self.text[self.cursor..].char_indices() {
            // A run of whitespace stops at the line break: `cw` on the trailing
            // spaces of a line must not swallow the newline.
            if c == '\n' || Some(char_class(c)) != class {
                break;
            }
            end = self.cursor + offset + c.len_utf8();
        }
        let deleted: String = self.text.drain(self.cursor..end).collect();
        self.history_pos = 0;
        self.suggestion = None;
        deleted
    }

    /// Delete from the cursor to the start of the next word and return the
    /// deleted text (Vim `dw`).
    ///
    /// The next word may be on a following line, in which case the line break
    /// goes too — the same join Vim performs.
    pub fn delete_to_word_forward(&mut self, count: usize) -> String {
        let mut end = self.cursor;
        for _ in 0..count.max(1) {
            end = self.word_forward_from(end);
        }
        if end <= self.cursor {
            return String::new();
        }
        let deleted: String = self.text.drain(self.cursor..end).collect();
        self.history_pos = 0;
        self.suggestion = None;
        deleted
    }

    /// Delete from the start of the previous word to the cursor and return the
    /// deleted text (Vim `db`).
    pub fn delete_to_word_backward(&mut self, count: usize) -> String {
        let mut start = self.cursor;
        for _ in 0..count.max(1) {
            start = self.word_backward_from(start);
        }
        if start >= self.cursor {
            return String::new();
        }
        let deleted: String = self.text.drain(start..self.cursor).collect();
        self.cursor = start;
        self.history_pos = 0;
        self.suggestion = None;
        deleted
    }

    /// Replace the Visual selection with `replacement`, and return the text
    /// that was replaced (Vim Visual `p`).
    ///
    /// Control characters are dropped from `replacement`, so paste-over cannot
    /// smuggle a stray escape or tab into the buffer. Line breaks are kept:
    /// the register may hold whole lines.
    ///
    /// Returns `None` when there is no selection.
    pub fn replace_selection(&mut self, replacement: &str) -> Option<String> {
        let (start, end) = self.selection_range()?;
        let replaced: String = self.text.drain(start..end).collect();
        let sanitized: String = replacement
            .chars()
            .filter(|c| !c.is_control() || *c == '\n')
            .collect();
        self.text.insert_str(start, &sanitized);
        self.cursor = start + sanitized.len();
        self.history_pos = 0;
        self.suggestion = None;
        self.clear_visual_selection();
        Some(replaced)
    }

    /// Add `levels` indentation steps to the start of the affected lines
    /// (Vim `>`).
    ///
    /// One step is [`INDENT_WIDTH`] spaces. In Visual mode every line the
    /// selection spans is indented; otherwise it is the cursor's line alone.
    /// The cursor keeps its position relative to the text by moving right past
    /// the inserted spaces.
    pub fn indent(&mut self, levels: usize) {
        if levels == 0 {
            return;
        }
        let prefix = " ".repeat(INDENT_WIDTH * levels);
        let mut shift = 0usize;
        for start in self.affected_line_starts() {
            // `start` is a pre-edit offset; earlier insertions have shifted the
            // text to its right.
            let at = start + shift;
            self.text.insert_str(at, &prefix);
            if at <= self.cursor {
                self.cursor += prefix.len();
            }
            shift += prefix.len();
        }
        self.suggestion = None;
    }

    /// Remove up to `levels` indentation steps from the start of the affected
    /// lines (Vim `<`). One tab counts as one step; otherwise up to
    /// [`INDENT_WIDTH`] leading spaces per step. A line with no leading
    /// whitespace is unchanged.
    pub fn outdent(&mut self, levels: usize) {
        let cursor_line = self.current_line().0;
        let mut shift = 0usize;
        for start in self.affected_line_starts() {
            let at = start - shift;
            let removed = self.remove_indent_at(at, levels);
            if at <= self.cursor {
                // A removal on the cursor's own line cannot pull the cursor
                // left past the line start; removals on earlier lines shift it
                // wholesale.
                let limit = if start == cursor_line {
                    self.cursor.saturating_sub(at)
                } else {
                    removed
                };
                self.cursor -= removed.min(limit);
            }
            shift += removed;
        }
        self.suggestion = None;
    }

    /// Remove up to `levels` indentation steps from the line starting at `at`.
    /// Returns how many bytes were removed.
    fn remove_indent_at(&mut self, at: usize, levels: usize) -> usize {
        let mut removed = 0;
        for _ in 0..levels {
            // Every step eats from the front of the line, so the cut point is
            // always `at` — the text shifts left underneath it.
            if self.text[at..].starts_with('\t') {
                self.text.remove(at);
                removed += 1;
                continue;
            }
            let spaces = self.text[at..]
                .chars()
                .take_while(|c| *c == ' ')
                .take(INDENT_WIDTH)
                .count();
            if spaces == 0 {
                break;
            }
            self.text.drain(at..at + spaces);
            removed += spaces;
        }
        removed
    }
}

/// Spaces added per indentation step by [`InputBuffer::indent`].
pub const INDENT_WIDTH: usize = 4;

/// The display width of `text`, counting a CJK character as two cells.
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(1))
        .sum()
}

/// The byte offset within `text` at display `column`, rounded down to a
/// character boundary. A column past the end of the text yields its length.
fn byte_at_column(text: &str, column: usize) -> usize {
    let mut width = 0;
    for (index, c) in text.char_indices() {
        let char_width = UnicodeWidthChar::width(c).unwrap_or(1);
        if width + char_width > column {
            return index;
        }
        width += char_width;
    }
    text.len()
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
        assert_eq!(buf.text, "");
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
        assert_eq!(buf.text, "");
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
    fn delete_word_end_deletes_the_word_run() {
        let mut buf = buffer_with("hello world");
        buf.cursor = 0;
        assert_eq!(buf.delete_word_end(), "hello");
        assert_eq!(buf.text, " world");
        assert_eq!(buf.cursor, 0);

        // On whitespace, `cw` deletes the whitespace run.
        buf.cursor = 0;
        assert_eq!(buf.delete_word_end(), " ");
        assert_eq!(buf.text, "world");

        // At end of buffer there is nothing to delete.
        buf.cursor = buf.text.len();
        assert_eq!(buf.delete_word_end(), "");
    }

    #[test]
    fn delete_word_end_handles_multibyte() {
        let mut buf = buffer_with("你好 world");
        buf.cursor = 0;
        assert_eq!(buf.delete_word_end(), "你好");
        assert_eq!(buf.text, " world");
        assert!(buf.text.is_char_boundary(buf.cursor));
    }

    #[test]
    fn replace_selection_swaps_the_range() {
        let mut buf = buffer_with("hello world");
        buf.cursor = 0;
        buf.start_visual(false);
        buf.cursor = 5;
        assert_eq!(buf.replace_selection("goodbye").as_deref(), Some("hello"));
        assert_eq!(buf.text, "goodbye world");
        assert_eq!(buf.cursor, 7);
        assert_eq!(buf.visual_anchor, None);
    }

    #[test]
    fn replace_selection_drops_control_characters() {
        let mut buf = buffer_with("hello");
        buf.cursor = 0;
        buf.start_visual(false);
        buf.cursor = 5;
        assert!(buf.replace_selection("a\tb\u{1b}c").is_some());
        assert_eq!(buf.text, "abc");
        assert!(buf.text.chars().all(|c| !c.is_control()));
    }

    /// A linewise register holds line breaks, so paste-over has to keep them —
    /// but still drop every other control character.
    #[test]
    fn replace_selection_keeps_line_breaks() {
        let mut buf = buffer_with("one\ntwo\nthree");
        buf.cursor = 0;
        buf.start_visual(true);
        buf.cursor = 7;
        assert_eq!(buf.selection_text(), Some("one\ntwo\n"));
        assert!(buf.replace_selection("first\nsecond\n").is_some());
        assert_eq!(buf.text, "first\nsecond\nthree");
    }

    #[test]
    fn replace_selection_without_a_selection_is_none() {
        let mut buf = buffer_with("hello");
        assert_eq!(buf.replace_selection("x"), None);
        assert_eq!(buf.text, "hello");
    }

    #[test]
    fn indent_and_outdent_the_line() {
        let mut buf = buffer_with("code");
        buf.indent(1);
        assert_eq!(buf.text, "    code");
        assert_eq!(buf.cursor, 8, "cursor keeps its place relative to the text");

        buf.indent(2);
        assert_eq!(buf.text, "            code");

        buf.outdent(1);
        assert_eq!(buf.text, "        code");
        buf.outdent(10);
        assert_eq!(buf.text, "code");
        // Outdenting past zero is a no-op, not an underflow.
        buf.outdent(1);
        assert_eq!(buf.text, "code");
    }

    #[test]
    fn outdent_removes_a_tab_as_one_step() {
        let mut buf = buffer_with("\tcode");
        buf.outdent(1);
        assert_eq!(buf.text, "code");
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

    // --- multi-line buffer model -------------------------------------------

    /// Cursor position `byte` inside `text`.
    fn buffer_at(text: &str, byte: usize) -> InputBuffer {
        let mut buf = InputBuffer::new();
        buf.text = text.to_string();
        buf.cursor = byte;
        buf
    }

    /// Everything a one-line buffer does must keep doing exactly what it did
    /// when the buffer was single-line by construction.
    #[test]
    fn single_line_buffer_degenerates_to_the_whole_buffer() {
        let buf = buffer_with("hello world");
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.current_line(), (0, 11));
        assert_eq!(buf.line_bounds_at(4), (0, 11));
        assert_eq!(buf.line_bounds_at(11), (0, 11));

        let empty = InputBuffer::new();
        assert_eq!(empty.line_count(), 1);
        assert_eq!(empty.current_line(), (0, 0));

        let mut buf = buffer_with("hello world");
        buf.cursor = 5;
        buf.apply(&InputAction::MoveToStart);
        assert_eq!(buf.cursor, 0);
        buf.apply(&InputAction::MoveToEnd);
        assert_eq!(buf.cursor, 11);

        buf.apply(&InputAction::MoveToStart);
        buf.apply(&InputAction::KillToEnd);
        assert_eq!(buf.text, "");
    }

    #[test]
    fn move_to_start_and_end_are_line_relative() {
        let mut buf = buffer_at("one\ntwo\nthree", 10);
        buf.apply(&InputAction::MoveToStart);
        assert_eq!(buf.cursor, 8);
        buf.apply(&InputAction::MoveToEnd);
        assert_eq!(buf.cursor, 13);
    }

    #[test]
    fn kill_to_end_and_start_stay_within_the_line() {
        let mut buf = buffer_at("one\ntwo\nthree", 5);
        buf.apply(&InputAction::KillToEnd);
        assert_eq!(buf.text, "one\nt\nthree");

        let mut buf = buffer_at("one\ntwo\nthree", 5);
        buf.apply(&InputAction::KillToStart);
        assert_eq!(buf.text, "one\nwo\nthree");
        assert_eq!(buf.cursor, 4);
    }

    #[test]
    fn delete_word_does_not_eat_the_line_break() {
        let mut buf = buffer_at("one two\nthree", 7);
        buf.apply(&InputAction::DeleteWord);
        assert_eq!(buf.text, "one \nthree");
        assert_eq!(buf.cursor, 4);
    }

    #[test]
    fn open_line_below_and_above() {
        let mut buf = buffer_at("one\ntwo", 5);
        buf.open_line_below();
        assert_eq!(buf.text, "one\ntwo\n");
        assert_eq!(buf.cursor, 8);
        assert_eq!(buf.line_count(), 3);

        let mut buf = buffer_at("one\ntwo", 5);
        buf.open_line_above();
        assert_eq!(buf.text, "one\n\ntwo");
        assert_eq!(buf.cursor, 4);

        // A single-line buffer gains a second line.
        let mut buf = buffer_with("solo");
        buf.open_line_below();
        assert_eq!(buf.text, "solo\n");
        assert_eq!(buf.cursor, 5);
    }

    #[test]
    fn move_line_keeps_the_display_column() {
        let mut buf = buffer_at("one\ntwo\nthree", 2);
        assert!(buf.move_line(1));
        assert_eq!(buf.cursor, 6); // "two" is only 3 wide
        assert!(buf.move_line(1));
        assert_eq!(buf.cursor, 10); // clamped to "three"'s 5 columns
        assert!(!buf.move_line(1), "no line below the last");
        assert!(buf.move_line(-1));
        assert_eq!(buf.cursor, 6);
        assert!(buf.move_line(-1));
        assert_eq!(buf.cursor, 2);
        assert!(!buf.move_line(-1), "no line above the first");

        // A single-line buffer has nowhere to go.
        let mut buf = buffer_with("solo");
        assert!(!buf.move_line(1));
        assert!(!buf.move_line(-1));
    }

    #[test]
    fn delete_lines_removes_the_break_too() {
        let mut buf = buffer_at("one\ntwo\nthree", 4);
        buf.delete_lines(1);
        assert_eq!(buf.text, "one\nthree");
        assert_eq!(buf.cursor, 4);

        // Deleting the last line takes the newline before it.
        let mut buf = buffer_at("one\ntwo\nthree", 9);
        buf.delete_lines(1);
        assert_eq!(buf.text, "one\ntwo");
        assert_eq!(buf.cursor, 4);

        let mut buf = buffer_at("one\ntwo\nthree", 4);
        buf.delete_lines(2);
        assert_eq!(buf.text, "one");
        assert_eq!(buf.cursor, 0);

        // `dd` on a single-line buffer still empties it.
        let mut buf = buffer_with("solo");
        buf.delete_lines(1);
        assert_eq!(buf.text, "");
        assert_eq!(buf.cursor, 0);
    }

    #[test]
    fn change_lines_leaves_one_empty_line() {
        let mut buf = buffer_at("one\ntwo\nthree", 4);
        buf.change_lines(1);
        assert_eq!(buf.text, "one\n\nthree");
        assert_eq!(buf.cursor, 4);

        let mut buf = buffer_at("one\ntwo\nthree", 4);
        buf.change_lines(2);
        assert_eq!(buf.text, "one\n");
        assert_eq!(buf.cursor, 4);

        let mut buf = buffer_with("solo");
        buf.change_lines(1);
        assert_eq!(buf.text, "");
        assert_eq!(buf.cursor, 0);
    }

    #[test]
    fn yank_lines_is_linewise_only_for_a_multi_line_buffer() {
        let buf = buffer_at("one\ntwo\nthree", 4);
        assert_eq!(buf.yank_lines(1), ("two\n".to_string(), true));
        assert_eq!(buf.yank_lines(2), ("two\nthree".to_string(), true));
        // Yanking does not touch the text.
        assert_eq!(buf.text, "one\ntwo\nthree");

        let buf = buffer_with("solo");
        assert_eq!(buf.yank_lines(1), ("solo".to_string(), false));
        assert_eq!(buf.yank_lines(3), ("solo".to_string(), false));
    }

    #[test]
    fn paste_keeps_line_breaks() {
        let mut buf = buffer_at("ab", 1);
        buf.paste("X\nY");
        assert_eq!(buf.text, "aX\nYb");
        assert_eq!(buf.cursor, 4);
    }

    #[test]
    fn paste_linewise_inserts_whole_lines() {
        let mut buf = buffer_at("one\ntwo", 0);
        buf.paste_linewise("three\n", 1, true);
        assert_eq!(buf.text, "one\nthree\ntwo");
        assert_eq!(buf.cursor, 4);

        let mut buf = buffer_at("one\ntwo", 4);
        buf.paste_linewise("three\n", 1, true);
        assert_eq!(buf.text, "one\ntwo\nthree\n");
        assert_eq!(buf.cursor, 8);

        let mut buf = buffer_at("one\ntwo", 4);
        buf.paste_linewise("zero\n", 2, true);
        assert_eq!(buf.text, "one\ntwo\nzero\nzero\n");
        assert_eq!(buf.cursor, 8);

        let mut buf = buffer_at("one\ntwo", 5);
        buf.paste_linewise("zero\n", 1, false);
        assert_eq!(buf.text, "one\nzero\ntwo");
    }

    #[test]
    fn linewise_selection_covers_whole_lines() {
        let mut buf = buffer_at("one\ntwo\nthree", 9);
        buf.start_visual(true);
        buf.cursor = 4;
        assert_eq!(buf.selection_range(), Some((4, 13)));
        assert_eq!(buf.selection_text(), Some("two\nthree"));

        // Backwards, and up to a middle line.
        let mut buf = buffer_at("one\ntwo\nthree", 4);
        buf.start_visual(true);
        buf.cursor = 9;
        assert_eq!(buf.selection_text(), Some("two\nthree"));

        let mut buf = buffer_at("one\ntwo\nthree", 5);
        buf.start_visual(true);
        assert_eq!(buf.selection_text(), Some("two\n"));

        // Deleting a linewise selection removes the lines outright.
        let mut buf = buffer_at("one\ntwo\nthree", 4);
        buf.start_visual(true);
        buf.cursor = 9;
        assert_eq!(buf.delete_selection().as_deref(), Some("two\nthree"));
        assert_eq!(buf.text, "one\n");
    }

    #[test]
    fn characterwise_selection_still_runs_between_anchor_and_cursor() {
        let mut buf = buffer_at("one\ntwo", 2);
        buf.start_visual(false);
        buf.cursor = 6;
        assert_eq!(buf.selection_text(), Some("e\ntw"));
    }

    #[test]
    fn indent_touches_only_the_current_line() {
        let mut buf = buffer_at("one\ntwo", 6);
        buf.indent(1);
        assert_eq!(buf.text, "one\n    two");
        assert_eq!(buf.cursor, 10);

        buf.outdent(1);
        assert_eq!(buf.text, "one\ntwo");
        assert_eq!(buf.cursor, 6);
    }

    #[test]
    fn outdent_clamps_the_cursor_to_the_line_start() {
        let mut buf = buffer_at("one\n    two", 4);
        buf.outdent(1);
        assert_eq!(buf.text, "one\ntwo");
        assert_eq!(buf.cursor, 4);
    }

    #[test]
    fn indent_a_linewise_selection_indents_every_line() {
        let mut buf = buffer_at("one\ntwo\nthree", 4);
        buf.start_visual(true);
        buf.cursor = 8;
        buf.indent(1);
        assert_eq!(buf.text, "one\n    two\n    three");
        // The cursor stays on the same character, shifted by both inserts.
        assert_eq!(buf.cursor, 16);
    }

    #[test]
    fn word_motions_cross_lines() {
        let mut buf = buffer_at("one two\nthree four", 0);
        buf.word_forward();
        assert_eq!(buf.cursor, 4);
        buf.word_forward();
        assert_eq!(buf.cursor, 8); // first word of the next line
        buf.word_end();
        assert_eq!(buf.cursor, 13);
        buf.word_backward();
        assert_eq!(buf.cursor, 8);
        buf.word_backward();
        assert_eq!(buf.cursor, 4);
    }

    #[test]
    fn delete_to_word_forward_joins_lines() {
        // From the start of "two", `dw` runs to the start of "three" on the
        // next line and takes the line break with it.
        let mut buf = buffer_at("one two\nthree", 4);
        assert_eq!(buf.delete_to_word_forward(1), "two\n");
        assert_eq!(buf.text, "one three");
        assert_eq!(buf.cursor, 4);
    }

    #[test]
    fn delete_to_word_backward_stops_at_the_word_start() {
        let mut buf = buffer_at("one\ntwo three", 7);
        assert_eq!(buf.delete_to_word_backward(1), "two");
        assert_eq!(buf.text, "one\n three");
        assert_eq!(buf.cursor, 4);
    }

    #[test]
    fn delete_word_end_stops_at_the_line_break() {
        let mut buf = buffer_at("one   \ntwo", 3);
        assert_eq!(buf.delete_word_end(), "   ");
        assert_eq!(buf.text, "one\ntwo");
    }

    // --- rendering model ---------------------------------------------------

    #[test]
    fn display_rows_follow_logical_lines() {
        let buf = buffer_with("hello\nworld");
        assert_eq!(buf.display_rows(80), vec![(0, 5), (6, 11)]);
        assert_eq!(buf.display_cursor(80), (1, 5));

        let buf = buffer_with("hello\n");
        assert_eq!(buf.display_rows(80), vec![(0, 5), (6, 6)]);
        assert_eq!(buf.display_cursor(80), (1, 0));

        // A short single line is still exactly one row, at column 0..len.
        let buf = buffer_with("hello");
        assert_eq!(buf.display_rows(80), vec![(0, 5)]);
        assert_eq!(buf.display_cursor(80), (0, 5));

        let buf = InputBuffer::new();
        assert_eq!(buf.display_rows(80), vec![(0, 0)]);
        assert_eq!(buf.display_cursor(80), (0, 0));
    }

    #[test]
    fn display_rows_wrap_long_lines() {
        let buf = buffer_with("abcdefgh");
        assert_eq!(buf.display_rows(4), vec![(0, 4), (4, 8), (8, 8)]);

        // The cursor wraps to the next row exactly at the boundary.
        let mut buf = buffer_with("abcdefgh");
        buf.cursor = 4;
        assert_eq!(buf.display_cursor(4), (1, 0));
        buf.cursor = 6;
        assert_eq!(buf.display_cursor(4), (1, 2));
        buf.cursor = 8;
        assert_eq!(buf.display_cursor(4), (2, 0));
    }

    #[test]
    fn display_rows_count_wide_characters_as_two_columns() {
        let mut buf = buffer_with("中文");
        // Each character is two columns wide.
        assert_eq!(buf.display_rows(4), vec![(0, 6), (6, 6)]);
        buf.cursor = 3;
        assert_eq!(buf.display_cursor(4), (0, 2));
    }

    #[test]
    fn display_column_is_measured_from_the_line_start() {
        let mut buf = buffer_with("one\ntwo");
        buf.cursor = 6;
        assert_eq!(buf.display_column(), 2);
        assert_eq!(buf.byte_from_column(1), 5);
    }
}
