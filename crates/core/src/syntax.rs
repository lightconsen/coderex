//! Syntax highlighting for code blocks and diffs.
//!
//! Wraps `syntect` for language-aware syntax highlighting.

use std::fmt::Write;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use thiserror::Error;

/// Errors from syntax highlighting.
#[derive(Debug, Error)]
pub enum SyntaxError {
    /// Language not supported.
    #[error("unsupported language: {0}")]
    UnsupportedLanguage(String),
    /// Theme not found.
    #[error("theme not found: {0}")]
    ThemeNotFound(String),
}

/// A single highlighted token.
#[derive(Debug, Clone)]
pub struct Token {
    /// The text content of the token.
    pub text: String,
    /// Foreground color as hex string.
    pub foreground: String,
    /// Whether the token is bold.
    pub is_bold: bool,
    /// Whether the token is italic.
    pub is_italic: bool,
}

/// Syntax highlighter with pre-loaded syntax and theme sets.
#[derive(Debug)]
pub struct SyntaxHighlighter {
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
    default_theme: String,
}

impl SyntaxHighlighter {
    /// Create a new syntax highlighter with default theme.
    pub fn new() -> Self {
        Self {
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set: ThemeSet::load_defaults(),
            default_theme: "base16-ocean.dark".into(),
        }
    }

    /// Create with a specific default theme.
    pub fn with_theme(theme: String) -> Result<Self, SyntaxError> {
        let theme_set = ThemeSet::load_defaults();
        if !theme_set.themes.contains_key(&theme) {
            return Err(SyntaxError::ThemeNotFound(theme));
        }
        Ok(Self {
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set,
            default_theme: theme,
        })
    }

    /// List available themes.
    #[must_use]
    pub fn available_themes(&self) -> Vec<String> {
        self.theme_set.themes.keys().cloned().collect()
    }

    /// List available syntaxes.
    #[must_use]
    pub fn available_syntaxes(&self) -> Vec<String> {
        self.syntax_set
            .syntaxes()
            .iter()
            .map(|s| s.name.clone())
            .collect()
    }

    /// Highlight source code for a given language.
    ///
    /// Returns a vector of lines, each containing a vector of tokens.
    pub fn highlight(&self, source: &str, language: &str) -> Result<Vec<Vec<Token>>, SyntaxError> {
        let syntax = self
            .syntax_set
            .find_syntax_by_name(language)
            .or_else(|| self.syntax_set.find_syntax_by_extension(language))
            .ok_or_else(|| SyntaxError::UnsupportedLanguage(language.to_string()))?;

        let theme = &self.theme_set.themes[&self.default_theme];
        let mut highlighter = HighlightLines::new(syntax, theme);

        let mut lines = Vec::new();
        for line in LinesWithEndings::from(source) {
            let tokens: Vec<Token> = highlighter
                .highlight_line(line, &self.syntax_set)
                .unwrap_or_default()
                .into_iter()
                .map(|(style, text)| style_to_token(style, text))
                .collect();
            lines.push(tokens);
        }

        Ok(lines)
    }

    /// Highlight and render as ANSI-colored text for terminal output.
    pub fn highlight_ansi(&self, source: &str, language: &str) -> Result<String, SyntaxError> {
        let tokens = self.highlight(source, language)?;

        let mut output = String::new();
        for line in tokens {
            for token in line {
                let color = hex_to_ansi(&token.foreground);
                if token.is_bold {
                    output.push_str("\x1b[1m");
                }
                if token.is_italic {
                    output.push_str("\x1b[3m");
                }
                if !token.foreground.is_empty() {
                    let _ = write!(output, "\x1b[38;5;{color}m");
                }
                output.push_str(&token.text);
                if token.is_bold || token.is_italic || !token.foreground.is_empty() {
                    output.push_str("\x1b[0m");
                }
            }
        }

        Ok(output)
    }

    /// Check if a language is supported.
    #[must_use]
    pub fn supports_language(&self, language: &str) -> bool {
        self.syntax_set
            .find_syntax_by_name(language)
            .or_else(|| self.syntax_set.find_syntax_by_extension(language))
            .is_some()
    }
}

impl Default for SyntaxHighlighter {
    fn default() -> Self {
        Self::new()
    }
}

fn style_to_token(style: Style, text: &str) -> Token {
    let fg = style.foreground;
    Token {
        text: text.to_string(),
        foreground: format!("{:02x}{:02x}{:02x}", fg.r, fg.g, fg.b),
        is_bold: style
            .font_style
            .contains(syntect::highlighting::FontStyle::BOLD),
        is_italic: style
            .font_style
            .contains(syntect::highlighting::FontStyle::ITALIC),
    }
}

/// Convert hex color to 256-color ANSI index (approximation).
#[allow(clippy::cast_possible_truncation)]
fn hex_to_ansi(hex: &str) -> u8 {
    if hex.len() != 6 {
        return 0;
    }
    // Simple conversion: use the red channel as a rough approximation
    // A full conversion would map to the nearest 256-color palette entry
    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);

    // Convert RGB to 256-color index
    if r == g && g == b {
        // Grayscale ramp
        if r < 8 {
            return 16;
        }
        if r > 238 {
            return 231;
        }
        232 + (r - 8) / 10
    } else {
        // Color cube
        let r_idx = (u16::from(r) * 5 + 127) / 255;
        let g_idx = (u16::from(g) * 5 + 127) / 255;
        let b_idx = (u16::from(b) * 5 + 127) / 255;
        (16 + r_idx * 36 + g_idx * 6 + b_idx) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlighter_lists_available_themes() {
        let highlighter = SyntaxHighlighter::new();
        let themes = highlighter.available_themes();

        assert!(!themes.is_empty());
        assert!(themes.iter().any(|t| t.contains("dark")));
    }

    #[test]
    fn highlighter_lists_available_syntaxes() {
        let highlighter = SyntaxHighlighter::new();
        let syntaxes = highlighter.available_syntaxes();

        assert!(!syntaxes.is_empty());
        assert!(syntaxes.contains(&"Rust".to_string()));
        assert!(syntaxes.contains(&"Python".to_string()));
    }

    #[test]
    fn highlight_rust_code() {
        let highlighter = SyntaxHighlighter::new();
        let source = "fn main() {\n    println!(\"Hello\");\n}";
        let tokens = highlighter.highlight(source, "Rust").unwrap();

        // Should have 3 lines
        assert_eq!(tokens.len(), 3);
        // First line should have tokens
        assert!(!tokens[0].is_empty());
    }

    #[test]
    fn highlight_python_code() {
        let highlighter = SyntaxHighlighter::new();
        let source = "def hello():\n    print('world')";
        let tokens = highlighter.highlight(source, "Python").unwrap();

        assert_eq!(tokens.len(), 2);
        assert!(!tokens[0].is_empty());
    }

    #[test]
    fn highlight_unsupported_language_returns_error() {
        let highlighter = SyntaxHighlighter::new();
        let result = highlighter.highlight("code", "NonexistentLanguage123");

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SyntaxError::UnsupportedLanguage(_)
        ));
    }

    #[test]
    fn supports_language_detects_known_languages() {
        let highlighter = SyntaxHighlighter::new();

        assert!(highlighter.supports_language("Rust"));
        assert!(highlighter.supports_language("Python"));
        assert!(highlighter.supports_language("JavaScript"));
        assert!(!highlighter.supports_language("TotallyMadeUp"));
    }

    #[test]
    fn highlight_ansi_returns_colored_output() {
        let highlighter = SyntaxHighlighter::new();
        let source = "fn main() {}";
        let ansi = highlighter.highlight_ansi(source, "Rust").unwrap();

        // Should contain ANSI escape codes
        assert!(ansi.contains("\x1b["));
    }

    #[test]
    fn theme_not_found_returns_error() {
        let result = SyntaxHighlighter::with_theme("nonexistent_theme_xyz".into());

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SyntaxError::ThemeNotFound(_)));
    }
}
