//! Statusline — renders model, tokens, cost, permissions, git branch.

/// Spinner frames for the thinking indicator (rotating).
const SPINNER: &[&str] = &["◐", "◓", "◑", "◒"];

/// Statusline data to render.
#[derive(Debug, Clone)]
pub struct Statusline {
    /// Current model name.
    pub model: String,
    /// Total tokens used (input + output).
    pub total_tokens: usize,
    /// Cost in USD.
    pub cost_usd: f64,
    /// Permission mode label.
    pub permission_mode: String,
    /// Git branch name.
    pub git_branch: String,
    /// Session title.
    pub session_title: String,
    /// Whether currently thinking/streaming.
    pub thinking: bool,
    /// Detailed thinking label (e.g. "Thinking (Esc to cancel)").
    pub thinking_label: Option<String>,
    /// Spinner animation frame index (incremented each draw while thinking).
    pub spinner_frame: u8,
    /// Show token count.
    pub show_tokens: bool,
    /// Show cost.
    pub show_cost: bool,
    /// Show git branch.
    pub show_git_branch: bool,
}

impl Statusline {
    /// Create a new statusline.
    #[must_use]
    pub fn new() -> Self {
        Self {
            model: String::new(),
            total_tokens: 0,
            cost_usd: 0.0,
            permission_mode: "default".into(),
            git_branch: String::new(),
            session_title: String::new(),
            thinking: false,
            thinking_label: None,
            spinner_frame: 0,
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
        }
    }

    /// Render to a single-line string.
    #[must_use]
    pub fn render(&self, width: usize) -> String {
        // If thinking with a label, show the spinner + label prominently.
        if self.thinking {
            let spinner = SPINNER[self.spinner_frame as usize % SPINNER.len()];
            let label = self
                .thinking_label
                .as_ref()
                .map_or("Thinking...".into(), |l| l.clone());
            let text = format!("{spinner} {label}");
            if text.len() <= width {
                // Pad to width so the statusline background fills evenly.
                let pad = width.saturating_sub(text.len());
                return format!("{text}{}", " ".repeat(pad));
            }
            return text[..width].to_string();
        }

        let mut segments: Vec<String> = vec![
            "○".into(),
            self.model.clone(),
            self.session_title.clone(),
        ];

        if self.show_tokens {
            segments.push(format!("tok:{},", self.total_tokens));
        }
        if self.show_cost {
            segments.push(format!("${:.2}", self.cost_usd));
        }
        if self.show_git_branch {
            segments.push(self.git_branch.clone());
        }
        segments.push(self.permission_mode.clone());

        let full = segments.join(" | ");
        if full.len() <= width {
            full
        } else {
            // Truncate by bytes, not chars, to ensure width constraint
            full[..width].to_string()
        }
    }
}

impl Default for Statusline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_basic() {
        let sl = Statusline {
            model: "sonnet".into(),
            total_tokens: 1234,
            cost_usd: 0.42,
            permission_mode: "approve".into(),
            git_branch: "main".into(),
            session_title: "Fix bug".into(),
            thinking: false,
            thinking_label: None,
            spinner_frame: 0,
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
        };
        let rendered = sl.render(200);
        assert!(rendered.contains("sonnet"));
        assert!(rendered.contains("tok:1234"));
        assert!(rendered.contains("$0.42"));
        assert!(rendered.contains("main"));
    }

    #[test]
    fn render_truncates() {
        let sl = Statusline {
            model: "a very long model name here".into(),
            total_tokens: 0,
            cost_usd: 0.0,
            permission_mode: "default".into(),
            git_branch: "main".into(),
            session_title: "".into(),
            thinking: false,
            thinking_label: None,
            spinner_frame: 0,
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
        };
        let rendered = sl.render(20);
        assert!(rendered.len() <= 20);
    }

    #[test]
    fn thinking_indicator_spins() {
        let sl = Statusline {
            thinking: true,
            thinking_label: Some("Thinking (Esc to cancel)".into()),
            spinner_frame: 0,
            ..Statusline::new()
        };
        let r0 = sl.render(200);
        assert!(r0.contains("◐"));

        let sl = Statusline {
            thinking: true,
            thinking_label: Some("Thinking (Esc to cancel)".into()),
            spinner_frame: 1,
            ..Statusline::new()
        };
        let r1 = sl.render(200);
        assert!(r1.contains("◓"));
    }

    #[test]
    fn thinking_replaces_normal_segments() {
        let sl = Statusline {
            thinking: true,
            thinking_label: Some("Running plan...".into()),
            spinner_frame: 0,
            model: "opus".into(),
            ..Statusline::new()
        };
        let rendered = sl.render(200);
        assert!(rendered.contains("Running plan..."));
        // Normal segments should not appear while thinking
        assert!(!rendered.contains("opus"));
    }
}
