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
        // Provider state, not conversation: never exported as visible text.
        MessageContent::Thinking(_) => String::new(),
        _ => "[unknown content]".into(),
    }
}

/// Whether an exported transcript should show this message at all.
const fn is_displayable(msg: &Message) -> bool {
    !matches!(
        msg.content,
        coderex_core::types::MessageContent::Thinking(_)
    )
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
    for msg in messages.iter().filter(|m| is_displayable(m)) {
        let _ = writeln!(out, "--- {} ---", role_label(msg.role));
        out.push_str(&content_text(&msg.content));
        out.push_str("\n\n");
    }
    out
}

fn format_markdown(messages: &[&Message]) -> String {
    let mut out = String::new();
    for msg in messages.iter().filter(|m| is_displayable(m)) {
        let _ = write!(out, "\n### {}\n\n", role_label(msg.role));
        match &msg.content {
            coderex_core::types::MessageContent::Text(text) => {
                out.push_str(text);
                out.push('\n');
            }
            coderex_core::types::MessageContent::ToolCall(tc) => {
                // Serialize rather than interpolate: a tool name or id with a
                // quote or newline must not produce invalid JSON in the fence.
                let payload = serde_json::json!({"tool": tc.name, "id": tc.id});
                let json = serde_json::to_string_pretty(&payload).unwrap_or_default();
                let _ = writeln!(out, "```json\n{json}\n```");
            }
            coderex_core::types::MessageContent::ToolResult(tr) => {
                let payload = serde_json::json!({"result": tr.output, "id": tr.tool_call_id});
                let json = serde_json::to_string_pretty(&payload).unwrap_or_default();
                let _ = writeln!(out, "```json\n{json}\n```");
            }
            // Provider state, not conversation.
            coderex_core::types::MessageContent::Thinking(_) => {}
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
        .filter(|m| is_displayable(m))
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

    /// Tool output is embedded in a Markdown JSON fence; quotes, newlines and
    /// backslashes must be escaped so the fence stays parseable. Ported from
    /// kimi-code's export-markdown contract cases.
    #[test]
    fn markdown_tool_fences_are_valid_json() {
        use coderex_core::types::{MessageContent, Role, ToolCall, ToolResult};

        let output = "first\nsecond \"quoted\" \\ backslash";
        let msgs = vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::ToolCall(ToolCall {
                    id: "call\"1".into(),
                    name: "Write".into(),
                    input: serde_json::json!({}),
                }),
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    tool_call_id: "call\"1".into(),
                    output: output.into(),
                    is_error: false,
                    images: Vec::new(),
                }),
            },
        ];
        let selector = MessageSelector::new(msgs.len());
        let markdown = export_messages(&msgs, &selector, ExportFormat::Markdown);

        let mut fences = markdown
            .split("```json")
            .skip(1)
            .map(|rest| rest.split("```").next().expect("closing fence"));
        let call: serde_json::Value = serde_json::from_str(fences.next().expect("call fence"))
            .expect("call fence is valid JSON");
        assert_eq!(call["tool"], "Write");
        assert_eq!(call["id"], "call\"1");

        let result: serde_json::Value = serde_json::from_str(fences.next().expect("result fence"))
            .expect("result fence is valid JSON");
        assert_eq!(result["result"], output);
        assert_eq!(result["id"], "call\"1");
    }

    #[test]
    fn json_export_preserves_quotes_newlines_and_unicode() {
        let msgs = vec![Message::assistant("quote \" line\n汉字")];
        let selector = MessageSelector::new(msgs.len());
        let output = export_messages(&msgs, &selector, ExportFormat::Json);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed[0]["content"], "quote \" line\n汉字");
    }
}
