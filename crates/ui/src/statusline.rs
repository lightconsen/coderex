//! Statusline — renders system status (thinking / elapsed time) above the input line.

use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

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
    /// Git branch name.
    pub git_branch: String,
    /// Session title.
    pub session_title: String,
    /// Whether currently thinking/streaming.
    pub thinking: bool,
    /// When the current thinking session started.
    pub thinking_started: Option<Instant>,
    /// Duration of the last completed turn.
    pub last_duration: Option<Duration>,
    /// Current plan execution status (e.g. "Step 2/5: Running tests").
    pub plan_status: Option<String>,
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
    /// Template from `statusline.format` in settings, when one is configured.
    ///
    /// Placeholders are written in brackets: `[model]`, `[title]`, `[tokens]`,
    /// `[cost]`, `[branch]`. An unknown placeholder is left as written, so a
    /// typo is visible instead of silently dropping a segment. An empty or
    /// whitespace-only template falls back to the built-in layout.
    pub format: Option<String>,
}

impl Statusline {
    /// Create a new statusline.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            model: String::new(),
            total_tokens: 0,
            cost_usd: 0.0,
            git_branch: String::new(),
            session_title: String::new(),
            thinking: false,
            thinking_started: None,
            last_duration: None,
            plan_status: None,
            thinking_label: None,
            spinner_frame: 0,
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
            format: None,
        }
    }

    /// Format a duration as "Xm Ys" or "Xs".
    #[must_use]
    fn fmt_duration(d: Duration) -> String {
        let secs = d.as_secs();
        let mins = secs / 60;
        let rem = secs % 60;
        if mins > 0 {
            format!("{mins}m {rem}s")
        } else {
            format!("{secs}s")
        }
    }

    /// Render to a single-line string.
    #[must_use]
    pub fn render(&self, width: usize) -> String {
        if self.thinking {
            let spinner = SPINNER[self.spinner_frame as usize % SPINNER.len()];
            let elapsed = self
                .thinking_started
                .map(|s| Self::fmt_duration(s.elapsed()))
                .unwrap_or_default();

            let label = self.thinking_label.as_ref().map_or_else(
                || {
                    self.plan_status.as_ref().map_or_else(
                        || {
                            if elapsed.is_empty() {
                                "Thinking...".into()
                            } else {
                                format!("Thinking...  ({elapsed})")
                            }
                        },
                        |plan| {
                            if elapsed.is_empty() {
                                plan.clone()
                            } else {
                                format!("{plan}  ({elapsed})")
                            }
                        },
                    )
                },
                |active| {
                    if elapsed.is_empty() {
                        active.clone()
                    } else {
                        format!("{active}  ({elapsed})")
                    }
                },
            );

            let text = format!("{spinner} {label}");
            let display_width = UnicodeWidthStr::width(text.as_str());
            if display_width <= width {
                let pad = width.saturating_sub(display_width);
                return format!("{text}{}", " ".repeat(pad));
            }
            return truncate_to_width(&text, width);
        }

        // After a turn completes, show elapsed time for a short while.
        if let Some(dur) = self.last_duration {
            let text = format!("Completed in {}", Self::fmt_duration(dur));
            let display_width = UnicodeWidthStr::width(text.as_str());
            if display_width <= width {
                let pad = width.saturating_sub(display_width);
                return format!("{text}{}", " ".repeat(pad));
            }
            return truncate_to_width(&text, width);
        }

        let mut segments: Vec<String> =
            vec!["○".into(), self.model.clone(), self.session_title.clone()];

        if self.show_tokens {
            segments.push(format!("tok:{},", self.total_tokens));
        }
        if self.show_cost {
            segments.push(format!("${:.2}", self.cost_usd));
        }
        if self.show_git_branch {
            segments.push(self.git_branch.clone());
        }

        let full = self
            .format
            .as_deref()
            .filter(|template| !template.trim().is_empty())
            .map_or_else(|| segments.join(" | "), |template| self.expand(template));
        if UnicodeWidthStr::width(full.as_str()) <= width {
            full
        } else {
            truncate_to_width(&full, width)
        }
    }

    /// Expand a `statusline.format` template.
    ///
    /// Only the documented placeholders are substituted; everything else —
    /// including an unknown `[placeholder]` — is copied through so a typo is
    /// visible rather than silently dropping a segment.
    fn expand(&self, template: &str) -> String {
        let tokens = self.total_tokens.to_string();
        let cost = format!("${:.2}", self.cost_usd);
        let mut out = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(open) = rest.find('[') {
            out.push_str(&rest[..open]);
            let Some(close) = rest[open..].find(']') else {
                out.push_str(&rest[open..]);
                return out;
            };
            let name = &rest[open + 1..open + close];
            match name {
                "model" => out.push_str(&self.model),
                "title" => out.push_str(&self.session_title),
                "tokens" => out.push_str(&tokens),
                "cost" => out.push_str(&cost),
                "branch" => out.push_str(&self.git_branch),
                _ => {
                    out.push('[');
                    out.push_str(name);
                    out.push(']');
                }
            }
            rest = &rest[open + close + 1..];
        }
        out.push_str(rest);
        out
    }
}

impl Default for Statusline {
    fn default() -> Self {
        Self::new()
    }
}

/// Truncate a string to fit within `max_width` display columns,
/// respecting grapheme boundaries.
fn truncate_to_width(s: &str, max_width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;

    let mut width = 0;
    let mut result = String::new();
    for grapheme in s.graphemes(true) {
        let gw = UnicodeWidthStr::width(grapheme);
        if width + gw > max_width {
            break;
        }
        width += gw;
        result.push_str(grapheme);
    }
    // Pad to exact width
    result.push_str(&" ".repeat(max_width.saturating_sub(width)));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_format_template_replaces_the_default_layout() {
        let sl = Statusline {
            model: "sonnet".into(),
            total_tokens: 1234,
            cost_usd: 0.42,
            git_branch: "main".into(),
            session_title: "Fix bug".into(),
            thinking: false,
            thinking_started: None,
            last_duration: None,
            plan_status: None,
            thinking_label: None,
            spinner_frame: 0,
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
            format: Some("[branch] > [model] ([tokens])".into()),
        };
        assert_eq!(sl.render(200), "main > sonnet (1234)");
    }

    #[test]
    fn an_unknown_placeholder_is_left_visible() {
        let sl = Statusline {
            model: "sonnet".into(),
            format: Some("[modle] [model]".into()),
            ..Statusline::new()
        };
        assert_eq!(sl.render(200), "[modle] sonnet");
    }

    #[test]
    fn an_empty_template_falls_back_to_the_default_layout() {
        let sl = Statusline {
            model: "sonnet".into(),
            format: Some("   ".into()),
            ..Statusline::new()
        };
        let rendered = sl.render(200);
        assert!(rendered.contains("sonnet"), "{rendered}");
        assert!(rendered.contains(" | "), "{rendered}");
    }

    #[test]
    fn a_long_template_is_truncated_to_the_width() {
        let sl = Statusline {
            session_title: "a very long session title that will not fit".into(),
            format: Some("[title]".into()),
            ..Statusline::new()
        };
        let rendered = sl.render(10);
        assert_eq!(UnicodeWidthStr::width(rendered.as_str()), 10);
    }

    #[test]
    fn render_basic() {
        let sl = Statusline {
            model: "sonnet".into(),
            total_tokens: 1234,
            cost_usd: 0.42,
            git_branch: "main".into(),
            session_title: "Fix bug".into(),
            thinking: false,
            thinking_started: None,
            last_duration: None,
            plan_status: None,
            thinking_label: None,
            spinner_frame: 0,
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
            format: None,
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
            git_branch: "main".into(),
            session_title: "".into(),
            thinking: false,
            thinking_started: None,
            last_duration: None,
            plan_status: None,
            thinking_label: None,
            spinner_frame: 0,
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
            format: None,
        };
        let rendered = sl.render(20);
        assert!(UnicodeWidthStr::width(rendered.as_str()) <= 20);
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

    #[test]
    fn truncation_respects_unicode_width() {
        let sl = Statusline {
            model: "日本語モデル".into(),
            total_tokens: 0,
            cost_usd: 0.0,
            git_branch: "main".into(),
            session_title: "".into(),
            thinking: false,
            thinking_started: None,
            last_duration: None,
            plan_status: None,
            thinking_label: None,
            spinner_frame: 0,
            show_tokens: true,
            show_cost: true,
            show_git_branch: true,
            format: None,
        };
        let rendered = sl.render(20);
        // Each CJK char is 2 columns wide, so truncation should handle correctly
        assert!(UnicodeWidthStr::width(rendered.as_str()) <= 20);
    }

    #[test]
    fn completed_duration_shows_after_completion() {
        let sl = Statusline {
            thinking: false,
            last_duration: Some(Duration::from_secs(7 * 60 + 27)),
            ..Statusline::new()
        };
        let rendered = sl.render(200);
        assert!(rendered.contains("Completed in"));
        assert!(rendered.contains("7m 27s"));
    }

    #[test]
    fn plan_status_shows_during_thinking() {
        let sl = Statusline {
            thinking: true,
            plan_status: Some("Step 2/5: Running tests".into()),
            spinner_frame: 0,
            ..Statusline::new()
        };
        let rendered = sl.render(200);
        assert!(rendered.contains("Step 2/5"));
        assert!(rendered.contains("Running tests"));
    }

    #[test]
    fn active_thinking_label_takes_priority_over_plan_status() {
        let sl = Statusline {
            thinking: true,
            thinking_label: Some("Thinking...".into()),
            plan_status: Some("Planning".into()),
            spinner_frame: 0,
            ..Statusline::new()
        };
        let rendered = sl.render(200);
        assert!(rendered.contains("Thinking..."));
        assert!(!rendered.contains("Planning"));
    }

    #[test]
    fn thinking_label_shows_elapsed_time() {
        let sl = Statusline {
            thinking: true,
            thinking_label: Some("Planning...".into()),
            thinking_started: Some(Instant::now() - Duration::from_secs(65)),
            spinner_frame: 0,
            ..Statusline::new()
        };
        let rendered = sl.render(200);
        assert!(rendered.contains("Planning..."));
        assert!(rendered.contains("1m "));
    }
}
