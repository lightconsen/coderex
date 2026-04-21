//! Message list — user/assistant/tool message rendering with virtualization.

use compact_str::CompactString;

/// Type of message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// User message.
    User,
    /// Assistant message.
    Assistant,
    /// Tool call result.
    ToolResult,
    /// Tool call request.
    ToolCall,
    /// System message.
    System,
}

/// A rendered message in the transcript.
#[derive(Debug, Clone)]
pub struct Message {
    /// Message role.
    pub role: MessageRole,
    /// Message content.
    pub content: CompactString,
    /// Tool name if role is ToolCall or ToolResult.
    pub tool_name: Option<CompactString>,
    /// Whether the message is currently streaming.
    pub streaming: bool,
    /// Whether the message is folded (collapsed).
    pub folded: bool,
}

/// Virtualized viewport into a message list.
#[derive(Debug)]
pub struct MessageView {
    /// All messages.
    pub messages: Vec<Message>,
    /// Index of the first visible message.
    pub start: usize,
    /// Number of messages visible.
    pub visible_count: usize,
    /// Total height of all messages (for scroll bar).
    pub total_height: usize,
}

impl MessageView {
    /// Create a new message view.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            messages: Vec::with_capacity(capacity),
            start: 0,
            visible_count: 0,
            total_height: 0,
        }
    }

    /// Add a message.
    pub fn push(&mut self, msg: Message) {
        self.messages.push(msg);
        // Auto-scroll to bottom if near bottom
        let at_bottom = self.start + self.visible_count >= self.messages.len().saturating_sub(1);
        if at_bottom {
            self.scroll_to_bottom();
        }
    }

    /// Scroll to the last message.
    pub fn scroll_to_bottom(&mut self) {
        let total = self.messages.len();
        self.start = total.saturating_sub(self.visible_count);
    }

    /// Scroll up by n messages.
    pub fn scroll_up(&mut self, n: usize) {
        self.start = self.start.saturating_sub(n);
    }

    /// Scroll down by n messages.
    pub fn scroll_down(&mut self) {
        let max_start = self.messages.len().saturating_sub(self.visible_count);
        self.start = (self.start + 1).min(max_start);
    }

    /// Get the visible messages.
    #[must_use]
    pub fn visible(&self) -> &[Message] {
        let end = (self.start + self.visible_count).min(self.messages.len());
        &self.messages[self.start..end]
    }

    /// Update the visible count (e.g., after resize).
    pub fn set_visible_count(&mut self, count: usize) {
        self.visible_count = count;
        // Keep start valid
        let max_start = self.messages.len().saturating_sub(self.visible_count);
        if self.start > max_start {
            self.start = max_start;
        }
    }

    /// Update the last message (for streaming).
    pub fn update_last(&mut self, content: CompactString) {
        if let Some(last) = self.messages.last_mut() {
            last.content = content;
        }
    }

    /// Check if the view is scrolled to the bottom.
    #[must_use]
    pub fn is_at_bottom(&self) -> bool {
        self.start + self.visible_count >= self.messages.len()
    }
}

/// Compute the display lines for a message given terminal width.
#[must_use]
pub fn message_display_lines(content: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let mut lines = 1;
    let mut col = 0;
    for c in content.chars() {
        if c == '\n' {
            lines += 1;
            col = 0;
        } else {
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(1);
            if col + w > width {
                lines += 1;
                col = w;
            } else {
                col += w;
            }
        }
    }
    lines.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: MessageRole, text: &str) -> Message {
        Message {
            role,
            content: CompactString::from(text),
            tool_name: None,
            streaming: false,
            folded: false,
        }
    }

    #[test]
    fn push_and_visible() {
        let mut view = MessageView::new(10);
        view.visible_count = 5;
        view.push(msg(MessageRole::User, "hello"));
        view.push(msg(MessageRole::Assistant, "world"));
        assert_eq!(view.messages.len(), 2);
        assert_eq!(view.visible().len(), 2);
    }

    #[test]
    fn scroll_up_down() {
        let mut view = MessageView::new(20);
        view.visible_count = 3;
        for i in 0..10 {
            view.push(msg(MessageRole::User, &format!("msg {i}")));
        }
        view.scroll_to_bottom();
        assert!(view.is_at_bottom());

        view.scroll_up(2);
        assert!(!view.is_at_bottom());
        assert_eq!(view.visible().len(), 3);
    }

    #[test]
    fn scroll_down_moves_toward_bottom() {
        let mut view = MessageView::new(20);
        view.visible_count = 3;
        for i in 0..5 {
            view.push(msg(MessageRole::User, &format!("msg {i}")));
        }
        view.scroll_to_bottom();
        assert!(view.is_at_bottom());
        view.scroll_up(2);
        let old_start = view.start;
        view.scroll_down();
        // scroll_down should move start toward the bottom
        assert!(view.start >= old_start);
    }

    #[test]
    fn update_last_streaming() {
        let mut view = MessageView::new(10);
        view.push(msg(MessageRole::Assistant, "Hel"));
        view.update_last(CompactString::from("Hello, world!"));
        assert_eq!(view.messages.last().unwrap().content, "Hello, world!");
    }

    #[test]
    fn resize_adjusts_start() {
        let mut view = MessageView::new(20);
        view.visible_count = 5;
        for i in 0..10 {
            view.push(msg(MessageRole::User, &format!("msg {i}")));
        }
        view.scroll_to_bottom();
        assert_eq!(view.start, 5);

        view.set_visible_count(8);
        assert!(view.start <= 2);
    }

    #[test]
    fn display_lines_simple() {
        assert_eq!(message_display_lines("hello", 80), 1);
        assert_eq!(message_display_lines("hello\nworld", 80), 2);
    }

    #[test]
    fn display_lines_wrap() {
        assert_eq!(message_display_lines("hello world foo", 10), 2);
        assert_eq!(message_display_lines("a\nb\nc", 80), 3);
    }

    #[test]
    fn at_bottom_after_push() {
        let mut view = MessageView::new(20);
        view.visible_count = 3;
        view.push(msg(MessageRole::User, "a"));
        view.push(msg(MessageRole::User, "b"));
        view.push(msg(MessageRole::User, "c"));
        view.push(msg(MessageRole::User, "d"));
        assert!(view.is_at_bottom());
    }
}
