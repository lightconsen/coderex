//! Statusline — renders model, tokens, cost, permissions, git branch.

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
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
        }
    }

    /// Render to a single-line string.
    #[must_use]
    pub fn render(&self, width: usize) -> String {
        let mut segments: Vec<String> = vec![
            if self.thinking {
                "●".into()
            } else {
                "○".into()
            },
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
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
        };
        let rendered = sl.render(20);
        assert!(rendered.len() <= 20);
    }

    #[test]
    fn thinking_indicator() {
        let sl_thinking = Statusline {
            thinking: true,
            ..Statusline::new()
        };
        let sl_idle = Statusline {
            thinking: false,
            ..Statusline::new()
        };
        assert!(sl_thinking.render(200).contains("●"));
        assert!(sl_idle.render(200).contains("○"));
    }
}
