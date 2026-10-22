//! Structured panels a plugin command can return to the person.
//!
//! A plugin command's response is normally a string. It may instead be a JSON
//! object:
//!
//! ```json
//! {
//!   "text": "optional summary printed before the panel",
//!   "ui": {
//!     "title": "Deploy status",
//!     "style": "info",          // info | warning | error
//!     "lines": ["api: ok", "web: failed"]
//!   }
//! }
//! ```
//!
//! The panel is rendered as text — a titled box in the terminal, and the same
//! box in a scrollable message in the TUI. CodeRex plugins are out-of-process,
//! so this is the UI surface they get: a structured, styled block rather than
//! a widget tree. Anything that is not a `ui` object is passed through
//! unchanged, so a plugin that prints JSON still prints JSON.

use serde_json::Value;

/// The widest panel line, in characters, before the border is clipped.
const MAX_PANEL_WIDTH: usize = 78;

/// Render a plugin command's response, turning a `ui` payload into a panel.
#[must_use]
pub fn render_plugin_response(output: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return output.to_string();
    };
    let Some(object) = value.as_object() else {
        return output.to_string();
    };
    let Some(ui) = object.get("ui").and_then(Value::as_object) else {
        return output.to_string();
    };

    let title = ui.get("title").and_then(Value::as_str).unwrap_or("");
    let style = ui.get("style").and_then(Value::as_str).unwrap_or("info");
    let lines: Vec<String> = ui
        .get("lines")
        .and_then(Value::as_array)
        .map(|lines| {
            lines
                .iter()
                .map(|line| line.as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default();

    let panel = render_panel(style, title, &lines);
    match object.get("text").and_then(Value::as_str) {
        Some(text) if !text.is_empty() => format!("{text}\n{panel}"),
        _ => panel,
    }
}

/// The style marker shown before a panel's title.
const fn style_marker(style: &str) -> &'static str {
    match style.as_bytes() {
        b"warning" => "! ",
        b"error" => "x ",
        _ => "",
    }
}

fn render_panel(style: &str, title: &str, lines: &[String]) -> String {
    let heading = format!("{}{title}", style_marker(style));
    let widest = lines
        .iter()
        .map(|line| line.chars().count())
        .chain(std::iter::once(heading.chars().count()))
        .max()
        .unwrap_or(0);
    let width = widest.min(MAX_PANEL_WIDTH);

    let top = if heading.is_empty() {
        format!("╭{}╮", "─".repeat(width + 2))
    } else {
        let rule = (width + 2).saturating_sub(heading.chars().count() + 2);
        format!("╭─ {heading} {}╮", "─".repeat(rule))
    };

    let mut out = String::with_capacity(top.len() + lines.len() * (width + 4));
    out.push_str(&top);
    for line in lines {
        let padding = width.saturating_sub(line.chars().count());
        out.push('\n');
        out.push_str("│ ");
        out.push_str(line);
        out.push_str(&" ".repeat(padding));
        out.push_str(" │");
    }
    out.push('\n');
    out.push('╰');
    out.push_str(&"─".repeat(width + 2));
    out.push('╯');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_string_is_passed_through() {
        assert_eq!(render_plugin_response("hello"), "hello");
    }

    #[test]
    fn json_without_a_ui_key_is_passed_through() {
        let output = r#"{"result":"ok"}"#;
        assert_eq!(render_plugin_response(output), output);
    }

    #[test]
    fn a_ui_payload_becomes_a_titled_panel() {
        let output = serde_json::json!({
            "text": "summary",
            "ui": {
                "title": "Deploy status",
                "lines": ["api: ok", "web: failed"],
            }
        })
        .to_string();
        let rendered = render_plugin_response(&output);
        assert!(
            rendered.starts_with("summary\n╭─ Deploy status "),
            "{rendered}"
        );
        assert!(rendered.contains("│ api: ok"), "{rendered}");
        assert!(rendered.contains("│ web: failed"), "{rendered}");
        assert!(rendered.ends_with('╯'), "{rendered}");
    }

    #[test]
    fn the_style_marker_is_shown() {
        let warning = serde_json::json!({
            "ui": {"title": "Careful", "style": "warning", "lines": []}
        })
        .to_string();
        assert!(render_plugin_response(&warning).contains("! Careful"));

        let error = serde_json::json!({
            "ui": {"title": "Broken", "style": "error", "lines": []}
        })
        .to_string();
        assert!(render_plugin_response(&error).contains("x Broken"));
    }
}
