//! Message export — export selected messages in various formats.

use crate::selector::MessageSelector;
use coderex_core::types::Message;
use std::fmt::Write as _;

/// Export format for messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// Plain text format.
    PlainText,
    /// Markdown format.
    Markdown,
    /// JSON format.
    Json,
}

/// Export selected messages as a formatted string.
#[must_use]
pub fn export_messages(
    messages: &[Message],
    selector: &MessageSelector,
    format: ExportFormat,
) -> String {
    let indices = selector.selected_indices();
    if indices.is_empty() {
        // Export all messages if none selected
        let refs: Vec<&Message> = messages.iter().collect();
        format_messages(&refs, format)
    } else {
        // Export only selected messages
        let selected: Vec<&Message> = indices.iter().filter_map(|&i| messages.get(i)).collect();
        format_messages(&selected, format)
    }
}

const fn role_label(role: coderex_core::types::Role) -> &'static str {
    use coderex_core::types::Role;
    match role {
        Role::User => "User",
        Role::Assistant => "Assistant",
        Role::System => "System",
        Role::Tool => "Tool",
        _ => "Unknown",
    }
}

const fn role_json(role: coderex_core::types::Role) -> &'static str {
    use coderex_core::types::Role;
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
        _ => "unknown",
    }
}

fn content_text(content: &coderex_core::types::MessageContent) -> String {
    use coderex_core::types::MessageContent;
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::ToolCall(tc) => format!("[tool_call: {}]", tc.name),
        MessageContent::ToolResult(tr) => format!("[tool_result: {}]", tr.output),
        _ => "[unknown content]".into(),
    }
}

fn format_messages(messages: &[&Message], format: ExportFormat) -> String {
    match format {
        ExportFormat::PlainText => format_plain(messages),
        ExportFormat::Markdown => format_markdown(messages),
        ExportFormat::Json => format_json(messages),
    }
}

fn format_plain(messages: &[&Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        let _ = writeln!(out, "--- {} ---", role_label(msg.role));
        out.push_str(&content_text(&msg.content));
        out.push_str("\n\n");
    }
    out
}

fn format_markdown(messages: &[&Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        let _ = write!(out, "\n### {}\n\n", role_label(msg.role));
        match &msg.content {
            coderex_core::types::MessageContent::Text(text) => {
                out.push_str(text);
                out.push('\n');
            }
            coderex_core::types::MessageContent::ToolCall(tc) => {
                let _ = write!(
                    out,
                    "```json\n{{\"tool\": \"{}\", \"id\": \"{}\"}}\n```\n",
                    tc.name, tc.id
                );
            }
            coderex_core::types::MessageContent::ToolResult(tr) => {
                let _ = write!(
                    out,
                    "```json\n{{\"result\": \"{}\", \"id\": \"{}\"}}\n```\n",
                    tr.output, tr.tool_call_id
                );
            }
            _ => out.push_str("[unknown]\n"),
        }
    }
    out
}

fn format_json(messages: &[&Message]) -> String {
    #[derive(serde::Serialize)]
    struct ExportMessage {
        role: String,
        content: String,
    }

    let export_msgs: Vec<ExportMessage> = messages
        .iter()
        .map(|m| ExportMessage {
            role: role_json(m.role).into(),
            content: content_text(&m.content),
        })
        .collect();

    serde_json::to_string_pretty(&export_msgs).unwrap_or_default()
}

/// Copy to clipboard — writes to the system clipboard if available.
#[cfg(target_os = "macos")]
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::Command;

    let mut child = Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
    }

    child.wait().map_err(|e| e.to_string())?;
    Ok(())
}

/// Copy to clipboard on Linux using xclip.
#[cfg(target_os = "linux")]
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::Command;

    let mut child = Command::new("xclip")
        .arg("-selection")
        .arg("clipboard")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
    }

    child.wait().map_err(|e| e.to_string())?;
    Ok(())
}

/// Copy to clipboard placeholder for other platforms.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn copy_to_clipboard(_text: &str) -> Result<(), String> {
    Err("Clipboard not supported on this platform".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::MessageSelector;

    fn test_messages() -> Vec<Message> {
        vec![Message::user("Hello"), Message::assistant("Hi there!")]
    }

    #[test]
    fn export_all_as_plain_text() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::PlainText);
        assert!(output.contains("User"));
        assert!(output.contains("Hello"));
        assert!(output.contains("Assistant"));
        assert!(output.contains("Hi there!"));
    }

    #[test]
    fn export_selected_as_markdown() {
        let msgs = test_messages();
        let mut selector = MessageSelector::new(msgs.len());
        // Navigate to first message and select it
        while selector.focus_index() > 0 {
            selector.focus_up();
        }
        selector.toggle_focus(); // Select first message only

        let output = export_messages(&msgs, &selector, ExportFormat::Markdown);
        assert!(output.contains("### User"));
        assert!(output.contains("Hello"));
        // Second message should not be present since only first is selected
        assert!(!output.contains("Hi there!"));
    }

    #[test]
    fn export_as_json() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::Json);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["role"], "user");
        assert_eq!(parsed[0]["content"], "Hello");
    }

    #[test]
    fn empty_selection_exports_all() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::PlainText);
        assert!(output.contains("Hello"));
        assert!(output.contains("Hi there!"));
    }

    #[test]
    fn empty_messages() {
        let msgs: Vec<Message> = Vec::new();
        let selector = MessageSelector::new(0);
        let output = export_messages(&msgs, &selector, ExportFormat::PlainText);
        assert!(output.is_empty());
    }

    #[test]
    fn plain_text_format_labels() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::PlainText);
        assert!(output.contains("--- User ---"));
        assert!(output.contains("--- Assistant ---"));
    }

    #[test]
    fn markdown_format_headings() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::Markdown);
        assert!(output.contains("### User"));
        assert!(output.contains("### Assistant"));
    }

    #[test]
    fn json_is_valid() {
        let msgs = test_messages();
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::Json);
        // Should not panic
        let _: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
    }
}
