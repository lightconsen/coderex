//! Markdown parsing and rendering.
//!
//! Wraps `comrak` for Markdown → HTML and Markdown → text conversion.

use comrak::{
    format_html, format_commonmark, parse_document, Arena, ComrakOptions,
};
use std::collections::HashMap;
use thiserror::Error;

/// Errors from Markdown processing.
#[derive(Debug, Error)]
pub enum MarkdownError {
    /// Failed to parse Markdown.
    #[error("parse error: {0}")]
    Parse(String),
    /// Failed to render HTML.
    #[error("render error: {0}")]
    Render(String),
}

/// Configuration for Markdown processing.
#[derive(Debug, Clone)]
pub struct MarkdownConfig {
    /// Enable GitHub-flavored markdown extensions.
    pub gfm: bool,
    /// Enable strikethrough.
    pub strikethrough: bool,
    /// Enable tables.
    pub tables: bool,
    /// Enable task lists.
    pub tasklists: bool,
    /// Enable syntax highlighting for code blocks.
    pub syntax_highlighting: bool,
    /// Enable autolinks.
    pub autolink: bool,
}

impl MarkdownConfig {
    /// Default configuration matching GitHub-flavored markdown.
    #[must_use]
    pub const fn gfm() -> Self {
        Self {
            gfm: true,
            strikethrough: true,
            tables: true,
            tasklists: true,
            syntax_highlighting: false,
            autolink: true,
        }
    }

    /// Minimal configuration for plain text rendering.
    #[must_use]
    pub const fn minimal() -> Self {
        Self {
            gfm: false,
            strikethrough: false,
            tables: false,
            tasklists: false,
            syntax_highlighting: false,
            autolink: false,
        }
    }
}

impl Default for MarkdownConfig {
    fn default() -> Self {
        Self::gfm()
    }
}

/// Convert Markdown to HTML.
pub fn markdown_to_html(
    input: &str,
    config: &MarkdownConfig,
) -> Result<String, MarkdownError> {
    let arena = Arena::new();
    let options = build_options(config);

    let root = parse_document(&arena, input, &options);

    let mut html = Vec::new();
    format_html(root, &options, &mut html)
        .map_err(|e| MarkdownError::Render(e.to_string()))?;

    String::from_utf8(html).map_err(|e| MarkdownError::Render(e.to_string()))
}

/// Convert Markdown back to CommonMark (roundtrip).
pub fn markdown_to_commonmark(
    input: &str,
    config: &MarkdownConfig,
) -> Result<String, MarkdownError> {
    let arena = Arena::new();
    let options = build_options(config);

    let root = parse_document(&arena, input, &options);

    let mut output = Vec::new();
    format_commonmark(root, &options, &mut output)
        .map_err(|e| MarkdownError::Render(e.to_string()))?;

    String::from_utf8(output).map_err(|e| MarkdownError::Render(e.to_string()))
}

/// Extract frontmatter from Markdown content (if present).
///
/// Frontmatter is YAML between `---` delimiters at the start of the file.
pub fn extract_frontmatter(input: &str) -> Option<HashMap<String, String>> {
    if !input.starts_with("---") {
        return None;
    }

    let rest = &input[3..];
    let end = rest.find("---")?;
    let yaml = &rest[..end].trim();

    let mut map = HashMap::new();
    for line in yaml.lines() {
        if let Some((key, value)) = line.split_once(':') {
            map.insert(
                key.trim().to_string(),
                value.trim().trim_matches('"').trim_matches('\'').to_string(),
            );
        }
    }

    Some(map)
}

/// Strip all Markdown formatting, returning plain text.
pub fn strip_markdown(input: &str) -> String {
    let mut result = input.to_string();

    // Remove code blocks
    let mut in_code = false;
    result = result
        .lines()
        .filter(|line| {
            if line.trim().starts_with("```") {
                in_code = !in_code;
                false
            } else {
                !in_code
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Remove inline code
    result = result.replace('`', "");

    // Remove headers
    result = result
        .lines()
        .map(|line| line.trim_start_matches('#').trim())
        .collect::<Vec<_>>()
        .join("\n");

    // Remove bold/italic
    result = result.replace("**", "").replace('*', "").replace("__", "").replace('_', "");

    // Remove links but keep text
    result = result.replace(['[', ']'], "");

    // Remove images
    result = result.replace("![", "").replace("](", "");

    // Remove blockquotes
    result = result
        .lines()
        .map(|line| line.trim_start_matches('>'))
        .collect::<Vec<_>>()
        .join("\n");

    result.trim().to_string()
}

fn build_options(config: &MarkdownConfig) -> ComrakOptions<'_> {
    let mut options = ComrakOptions::default();
    options.extension.strikethrough = config.strikethrough;
    options.extension.table = config.tables;
    options.extension.tasklist = config.tasklists;
    options.extension.autolink = config.autolink;
    options.render.hardbreaks = true;
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_to_html_renders_basic() {
        let input = "# Hello\n\nThis is **bold** text.";
        let html = markdown_to_html(input, &MarkdownConfig::gfm()).unwrap();

        assert!(html.contains("<h1"));
        assert!(html.contains("<strong>bold</strong>"));
    }

    #[test]
    fn markdown_to_html_renders_tables() {
        let input = "| A | B |\n|---|---|\n| 1 | 2 |";
        let html = markdown_to_html(input, &MarkdownConfig::gfm()).unwrap();

        assert!(html.contains("<table"));
    }

    #[test]
    fn markdown_to_html_renders_tasklists() {
        let input = "- [x] Done\n- [ ] Todo";
        let html = markdown_to_html(input, &MarkdownConfig::gfm()).unwrap();

        assert!(html.contains("checkbox"));
    }

    #[test]
    fn markdown_to_commonmark_roundtrip() {
        let input = "# Hello\n\nSome text.";
        let roundtrip = markdown_to_commonmark(input, &MarkdownConfig::gfm()).unwrap();

        // Should produce valid CommonMark that can be re-parsed
        assert!(roundtrip.contains("Hello"));
        assert!(roundtrip.contains("Some text"));
    }

    #[test]
    fn extract_frontmatter_parses_yaml() {
        let input = "---\ntitle: My Post\nauthor: Alice\n---\n\nContent here.";
        let fm = extract_frontmatter(input).unwrap();

        assert_eq!(fm.get("title"), Some(&"My Post".to_string()));
        assert_eq!(fm.get("author"), Some(&"Alice".to_string()));
    }

    #[test]
    fn extract_frontmatter_returns_none_without_delimiters() {
        let input = "Just content, no frontmatter.";
        assert!(extract_frontmatter(input).is_none());
    }

    #[test]
    fn strip_markdown_removes_formatting() {
        let input = "# Header\n\n**Bold** and *italic* text.\n\n- [ ] Task\n\n`code`\n\n[link](url)";
        let plain = strip_markdown(input);

        assert!(plain.contains("Header"));
        assert!(plain.contains("Bold"));
        assert!(plain.contains("italic"));
        assert!(!plain.contains("**"));
        assert!(!plain.contains("```"));
    }
}
