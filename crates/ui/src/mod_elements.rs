//! Render a mod's declarative UI tree into terminal text.
//!
//! The renderer is a pure transformation: it takes the host-owned
//! [`UiElement`] snapshot plus a width and returns styled [`Line`]s. It keeps
//! no TUI state, so the same tree can be rendered into the interactive
//! transcript or, through [`render_mod_ui_text`], into a headless run's output.
//!
//! Buttons render as `[label]`. The caller may pass the id of the focused
//! button; that button renders as `> [label] <` with reversed emphasis, while
//! every other button keeps the plain form. The renderer still keeps no state:
//! focus is an argument, not something cached here.

use coderex_core::ui_protocol::{UiElement, UiStyle};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Width used when a caller has no terminal measurement to offer.
pub const MOD_UI_DEFAULT_WIDTH: usize = 80;

/// Map a mod's [`UiStyle`] onto the terminal UI's style vocabulary.
///
/// `Warning` and `Error` use the same colors as the notification center, and
/// `Bold`/`Dim` the same modifiers as the rest of the UI.
#[must_use]
pub const fn ui_style(style: UiStyle) -> Style {
    match style {
        UiStyle::Plain => Style::new(),
        UiStyle::Bold => Style::new().add_modifier(Modifier::BOLD),
        UiStyle::Dim => Style::new().add_modifier(Modifier::DIM),
        UiStyle::Warning => Style::new().fg(Color::Yellow),
        UiStyle::Error => Style::new().fg(Color::Red),
    }
}

/// Render a mod UI tree into styled terminal lines at most `width` columns wide.
///
/// Boxes nest with box-drawing borders and an optional title. Text wraps at
/// `width`; buttons render as `[label]`. The result is a snapshot: nothing
/// here refers back to the mod or the engine.
#[must_use]
pub fn render_mod_ui(elements: &[UiElement], width: usize) -> Vec<Line<'static>> {
    render_mod_ui_with_focus(elements, width, None)
}

/// Render a mod UI tree, giving one button the focused treatment.
///
/// The button whose id equals `focused` renders as `> [label] <` with reversed
/// emphasis; every other button renders as `[label]`. `None` is exactly
/// [`render_mod_ui`].
#[must_use]
pub fn render_mod_ui_with_focus(
    elements: &[UiElement],
    width: usize,
    focused: Option<&str>,
) -> Vec<Line<'static>> {
    render_children(elements, width.max(1), focused)
}

/// Render a mod UI tree as a plain text block, one line per row.
///
/// This is the headless/transcript form of [`render_mod_ui`]; styling is
/// intentionally dropped because both consumers store plain text.
#[must_use]
pub fn render_mod_ui_text(elements: &[UiElement], width: usize) -> String {
    render_mod_ui_text_with_focus(elements, width, None)
}

/// The plain-text form of [`render_mod_ui_with_focus`].
#[must_use]
pub fn render_mod_ui_text_with_focus(
    elements: &[UiElement],
    width: usize,
    focused: Option<&str>,
) -> String {
    render_mod_ui_with_focus(elements, width, focused)
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn line_text(line: &Line<'static>) -> String {
    let mut text = String::new();
    for span in &line.spans {
        text.push_str(span.content.as_ref());
    }
    text
}

fn render_children(
    elements: &[UiElement],
    width: usize,
    focused: Option<&str>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for element in elements {
        lines.extend(render_element(element, width, focused));
    }
    lines
}

fn render_element(element: &UiElement, width: usize, focused: Option<&str>) -> Vec<Line<'static>> {
    match element {
        UiElement::Text { text, style } => {
            let style = ui_style(*style);
            wrap_text(text, width)
                .into_iter()
                .map(|text| Line::from(Span::styled(text, style)))
                .collect()
        }
        UiElement::Button { label, id } => {
            let is_focused = focused == Some(id.as_str());
            let text = if is_focused {
                format!("> [{label}] <")
            } else {
                format!("[{label}]")
            };
            let text = truncate_to_width(&text, width);
            let style = if is_focused {
                ui_style(UiStyle::Bold).add_modifier(Modifier::REVERSED)
            } else {
                ui_style(UiStyle::Bold)
            };
            vec![Line::from(Span::styled(text, style))]
        }
        UiElement::Box { title, children } => {
            render_box(title.as_deref(), children, width, focused)
        }
    }
}

/// A box needs two border columns, two padding columns and at least one
/// content column. Narrower than that, the borders would be noise, so the
/// children render on their own.
const MIN_BOX_WIDTH: usize = 5;

fn render_box(
    title: Option<&str>,
    children: &[UiElement],
    width: usize,
    focused: Option<&str>,
) -> Vec<Line<'static>> {
    if width < MIN_BOX_WIDTH {
        return render_children(children, width, focused);
    }

    let border = Style::new().fg(Color::DarkGray);
    let inner_width = width - 4;
    let mut lines = vec![top_border(title, width, border)];
    let child_lines = render_children(children, inner_width, focused);
    if child_lines.is_empty() {
        lines.push(content_line(Line::default(), inner_width, border));
    } else {
        lines.extend(
            child_lines
                .into_iter()
                .map(|line| content_line(line, inner_width, border)),
        );
    }
    lines.push(simple_border('└', '┘', width, border));
    lines
}

fn top_border(title: Option<&str>, width: usize, style: Style) -> Line<'static> {
    let Some(title) = title.filter(|title| !title.is_empty()) else {
        return simple_border('┌', '┐', width, style);
    };

    // `┌─ ` + title + ` ` + fill + `┐`
    let available = width.saturating_sub(5);
    if available == 0 {
        return simple_border('┌', '┐', width, style);
    }
    let title = truncate_to_width(title, available);
    let fill = available.saturating_sub(display_width(&title));

    let mut text = String::from("┌─ ");
    text.push_str(&title);
    text.push(' ');
    text.extend(std::iter::repeat_n('─', fill));
    text.push('┐');
    Line::from(Span::styled(text, style))
}

fn simple_border(left: char, right: char, width: usize, style: Style) -> Line<'static> {
    let mut text = String::new();
    text.push(left);
    text.extend(std::iter::repeat_n('─', width.saturating_sub(2)));
    text.push(right);
    Line::from(Span::styled(text, style))
}

fn content_line(content: Line<'static>, inner_width: usize, border: Style) -> Line<'static> {
    let content_width = content
        .spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum::<usize>();
    let padding = inner_width.saturating_sub(content_width);

    let mut spans = Vec::with_capacity(content.spans.len() + 3);
    spans.push(Span::styled("│ ", border));
    spans.extend(content.spans);
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
    }
    spans.push(Span::styled(" │", border));
    Line::from(spans)
}

/// Wrap `text` at `width` columns, preserving explicit newlines.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();

    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }

        let mut current = String::new();
        let mut current_width = 0;
        for word in paragraph.split_whitespace() {
            let word_width = display_width(word);
            if word_width > width {
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                }
                let mut remaining = word.to_string();
                while display_width(&remaining) > width {
                    let (head, tail) = split_at_width(&remaining, width);
                    lines.push(head);
                    remaining = tail;
                }
                current_width = display_width(&remaining);
                current = remaining;
            } else if current.is_empty() {
                current.push_str(word);
                current_width = word_width;
            } else if current_width + 1 + word_width <= width {
                current.push(' ');
                current.push_str(word);
                current_width += 1 + word_width;
            } else {
                lines.push(std::mem::take(&mut current));
                current.push_str(word);
                current_width = word_width;
            }
        }
        lines.push(current);
    }

    lines
}

/// Split `text` after the last character that fits in `width` columns.
///
/// At least one character is always taken, so a glyph wider than `width`
/// cannot make the caller loop forever.
fn split_at_width(text: &str, width: usize) -> (String, String) {
    let mut used = 0;
    let mut boundary = text.len();
    for (index, character) in text.char_indices() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > width && used > 0 {
            boundary = index;
            break;
        }
        used += character_width;
    }
    let boundary = boundary.max(text.chars().next().map_or(0, char::len_utf8));
    let (head, tail) = text.split_at(boundary);
    (head.to_string(), tail.to_string())
}

fn truncate_to_width(text: &str, width: usize) -> String {
    let mut truncated = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > width {
            break;
        }
        truncated.push(character);
        used += character_width;
    }
    truncated
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> UiElement {
        UiElement::Text {
            text: text.to_string(),
            style: UiStyle::Plain,
        }
    }

    fn button(label: &str) -> UiElement {
        UiElement::Button {
            label: label.to_string(),
            id: label.to_ascii_lowercase(),
        }
    }

    #[test]
    fn text_and_buttons_render_in_order() {
        let elements = vec![text("hello"), button("Retry")];
        assert_eq!(render_mod_ui_text(&elements, 20), "hello\n[Retry]");
    }

    #[test]
    fn buttons_are_labelled_affordances_not_interactive_widgets() {
        let lines = render_mod_ui(&[button("Run")], 20);
        let rendered = line_text(&lines[0]);
        assert_eq!(rendered, "[Run]");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD),
            "a button should read as an affordance"
        );
    }

    #[test]
    fn a_focused_button_is_marked_and_reversed() {
        let lines = render_mod_ui_with_focus(&[button("Run")], 20, Some("run"));
        assert_eq!(line_text(&lines[0]), "> [Run] <");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED),
            "the focused button should be visually distinct"
        );
    }

    #[test]
    fn only_the_focused_button_changes_its_text() {
        let elements = [button("Run"), button("Stop")];
        let rendered = render_mod_ui_text_with_focus(&elements, 20, Some("stop"));
        assert_eq!(rendered, "[Run]\n> [Stop] <");
        assert_eq!(
            render_mod_ui_text(&elements, 20),
            "[Run]\n[Stop]",
            "no focus keeps the existing output"
        );
    }

    #[test]
    fn focus_reaches_buttons_nested_in_boxes() {
        let tree = vec![UiElement::Box {
            title: Some("Panel".to_string()),
            children: vec![button("Go")],
        }];
        let rendered = render_mod_ui_text_with_focus(&tree, 24, Some("go"));
        assert!(rendered.contains("> [Go] <"), "{rendered}");
    }

    #[test]
    fn nested_boxes_snapshot() {
        let tree = vec![UiElement::Box {
            title: Some("Outer".to_string()),
            children: vec![
                text("hi"),
                UiElement::Box {
                    title: Some("Inner".to_string()),
                    children: vec![button("Go")],
                },
            ],
        }];

        let rendered = render_mod_ui_text(&tree, 24);
        let expected = "\
┌─ Outer ──────────────┐
│ hi                   │
│ ┌─ Inner ──────────┐ │
│ │ [Go]             │ │
│ └──────────────────┘ │
└──────────────────────┘";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn a_box_without_a_title_or_children_still_has_a_frame() {
        let tree = vec![UiElement::Box {
            title: None,
            children: Vec::new(),
        }];
        assert_eq!(
            render_mod_ui_text(&tree, 12),
            "┌──────────┐\n│          │\n└──────────┘"
        );
    }

    #[test]
    fn styles_map_to_the_terminal_vocabulary() {
        assert_eq!(ui_style(UiStyle::Plain), Style::new());
        assert!(
            ui_style(UiStyle::Bold)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(ui_style(UiStyle::Dim).add_modifier.contains(Modifier::DIM));
        assert_eq!(ui_style(UiStyle::Warning).fg, Some(Color::Yellow));
        assert_eq!(ui_style(UiStyle::Error).fg, Some(Color::Red));
    }

    #[test]
    fn a_text_style_reaches_the_rendered_spans() {
        let lines = render_mod_ui(
            &[UiElement::Text {
                text: "careful".to_string(),
                style: UiStyle::Warning,
            }],
            20,
        );
        assert_eq!(lines[0].spans[0].content.as_ref(), "careful");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn text_wraps_to_the_given_width() {
        let tree = vec![text("one two three four")];
        assert_eq!(render_mod_ui_text(&tree, 9), "one two\nthree\nfour");
    }

    #[test]
    fn long_words_and_narrow_buttons_are_clipped_to_the_width() {
        let tree = vec![text("abcdefghij"), button("Very long label")];
        let rendered = render_mod_ui_text(&tree, 5);
        for line in rendered.lines() {
            assert!(display_width(line) <= 5, "{line:?} is wider than 5 columns");
        }
    }

    #[test]
    fn box_lines_share_one_width() {
        let tree = vec![UiElement::Box {
            title: Some("T".to_string()),
            children: vec![text("a longer line of text"), button("Ok")],
        }];
        let rendered = render_mod_ui_text(&tree, 16);
        for line in rendered.lines() {
            assert_eq!(display_width(line), 16, "{line:?}");
        }
    }
}
